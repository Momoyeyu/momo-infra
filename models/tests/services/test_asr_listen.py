"""/v1/listen 集成测试：Deepgram 对齐对外形态——Metadata/VAD 事件/Results、
Finalize/CloseStream、参数与鉴权（provider-protocol.md §1）。"""

import contextlib

import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

import momo_models.services.asr_service as asr_service
from momo_models.config import Settings
from momo_models.providers.base import (
    SPEECH_END,
    SPEECH_START,
    ASRProvider,
    Transcript,
    VadEvent,
    VADProvider,
)


class _FakeVad(VADProvider):
    name = "fake-vad"

    def __init__(self, script=None, flush_events=None) -> None:
        self.script = list(script or [])
        self.flush_events = list(flush_events or [])
        self.fed: list[bytes] = []

    async def load(self) -> None: ...

    def feed(self, pcm: bytes):
        self.fed.append(pcm)
        return iter(self.script.pop(0) if self.script else [])

    def flush(self):
        return iter(self.flush_events)

    def reset(self) -> None:
        self.fed.clear()


class _FakeAsr(ASRProvider):
    name = "fake-asr"

    def __init__(self, text: str = "你好，世界。") -> None:
        self.text = text
        self.calls: list[dict] = []

    async def load(self) -> None: ...

    async def transcribe(self, pcm: bytes, *, lang: str | None) -> Transcript:
        self.calls.append({"pcm": pcm, "lang": lang})
        return Transcript(text=self.text, lang="zh", duration_ms=len(pcm) // 32)


@contextlib.contextmanager
def _app_with(
    monkeypatch: pytest.MonkeyPatch,
    asr: ASRProvider,
    vad: VADProvider,
    *,
    api_key: str = "",
):
    monkeypatch.setattr(asr_service, "create_asr_provider", lambda s: asr)
    monkeypatch.setattr(asr_service, "create_vad_provider", lambda s: vad)
    app = asr_service.create_app()
    app.state.settings = Settings(asr_api_key=api_key)
    with TestClient(app) as client:
        yield client


def _pcm(samples: int) -> bytes:
    return b"\x01\x00" * samples


def test_listen_metadata_then_results(monkeypatch: pytest.MonkeyPatch) -> None:
    """binary PCM → SpeechStarted/UtteranceEnd/Results（final 转写）。"""
    seg_pcm = _pcm(1600)
    vad = _FakeVad(
        script=[
            [VadEvent(event=SPEECH_START, at_ms=1240, segment_index=1)],
            [VadEvent(event=SPEECH_END, at_ms=3860, segment_index=1, pcm=seg_pcm)],
        ]
    )
    asr = _FakeAsr(text="你好")
    with (
        _app_with(monkeypatch, asr, vad) as client,
        client.websocket_connect("/v1/listen?encoding=linear16&sample_rate=16000&channels=1") as ws,
    ):
        meta = ws.receive_json()
        assert meta["type"] == "Metadata"
        assert meta["request_id"]
        assert meta["sample_rate"] == 16000

        ws.send_bytes(_pcm(800))
        started = ws.receive_json()
        assert started == {"type": "SpeechStarted", "timestamp": 1.24}

        ws.send_bytes(_pcm(800))
        end = ws.receive_json()
        assert end["type"] == "UtteranceEnd"
        assert end["last_word_end"] == 3.86
        results = ws.receive_json()
        assert results["type"] == "Results"
        assert results["is_final"] is True
        assert results["speech_final"] is True
        alt = results["channel"]["alternatives"][0]
        assert alt["transcript"] == "你好"
        assert asr.calls == [{"pcm": seg_pcm, "lang": None}]


def test_listen_finalize_flushes_tail(monkeypatch: pytest.MonkeyPatch) -> None:
    """Finalize → vad.flush 残留段转写成 Results。"""
    tail = _pcm(800)
    vad = _FakeVad(flush_events=[VadEvent(event=SPEECH_END, at_ms=500, segment_index=1, pcm=tail)])
    asr = _FakeAsr()
    with _app_with(monkeypatch, asr, vad) as client, client.websocket_connect("/v1/listen") as ws:
        assert ws.receive_json()["type"] == "Metadata"
        ws.send_bytes(_pcm(800))
        ws.send_json({"type": "Finalize"})
        end = ws.receive_json()
        assert end["type"] == "UtteranceEnd"
        results = ws.receive_json()
        assert results["type"] == "Results"
        assert asr.calls[0]["pcm"] == tail


def test_listen_close_stream_acks_and_closes(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/v1/listen") as ws,
    ):
        assert ws.receive_json()["type"] == "Metadata"
        ws.send_json({"type": "CloseStream"})
        ack = ws.receive_json()
        assert ack == {"type": "CloseStream"}
        close = ws.receive()
        assert close["type"] == "websocket.close"
        assert close["code"] == 1000


def test_listen_unknown_control_reports_error(monkeypatch: pytest.MonkeyPatch) -> None:
    with (
        _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client,
        client.websocket_connect("/v1/listen") as ws,
    ):
        assert ws.receive_json()["type"] == "Metadata"
        ws.send_json({"type": "Nope"})
        err = ws.receive_json()
        assert err["type"] == "Error"
        assert err["code"] == "BAD_MESSAGE"
        ws.send_text("not json")
        err = ws.receive_json()
        assert err["type"] == "Error"


def test_listen_bad_params_rejected(monkeypatch: pytest.MonkeyPatch) -> None:
    with _app_with(monkeypatch, _FakeAsr(), _FakeVad()) as client:
        with pytest.raises(WebSocketDisconnect) as ei:
            with client.websocket_connect("/v1/listen?encoding=opus"):
                pass
        assert ei.value.code == 4400


def test_listen_auth_enforced_when_key_set(monkeypatch: pytest.MonkeyPatch) -> None:
    """MOMO_ASR_API_KEY 已设：无凭据 4401；?token= 与 Token 头均可。"""
    with _app_with(monkeypatch, _FakeAsr(), _FakeVad(), api_key="k1") as client:
        with pytest.raises(WebSocketDisconnect) as ei:
            with client.websocket_connect("/v1/listen"):
                pass
        assert ei.value.code == 4401

        with client.websocket_connect("/v1/listen?token=k1") as ws:
            assert ws.receive_json()["type"] == "Metadata"

        with client.websocket_connect("/v1/listen", headers={"Authorization": "Token k1"}) as ws:
            assert ws.receive_json()["type"] == "Metadata"

        with pytest.raises(WebSocketDisconnect) as ei:
            with client.websocket_connect("/v1/listen?token=wrong"):
                pass
        assert ei.value.code == 4401


def test_listen_provider_not_ready(monkeypatch: pytest.MonkeyPatch) -> None:
    class _FailVad(_FakeVad):
        async def load(self) -> None:
            raise RuntimeError("no model")

    with (
        _app_with(monkeypatch, _FakeAsr(), _FailVad()) as client,
        client.websocket_connect("/v1/listen") as ws,
    ):
        err = ws.receive_json()
        assert err["type"] == "Error"
        assert err["code"] == "ASR_FAILED"
        close = ws.receive()
        assert close["type"] == "websocket.close"
        assert close["code"] == 1011
