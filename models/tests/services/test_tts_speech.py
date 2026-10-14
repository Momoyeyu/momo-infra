"""POST /v1/audio/speech 集成测试：OpenAI 对齐对外形态——pcm 流式、wav 整段、
字段校验、鉴权与 provider 状态（provider-protocol.md §2）。"""

import contextlib

from fastapi.testclient import TestClient

from momo_models.audio import wav_unwrap
from momo_models.config import Settings
from momo_models.providers.base import TTSCaps, TTSProvider, VoiceRef
from momo_models.services.tts_service import create_app


class _FakeTTS(TTSProvider):
    name = "fake"

    def __init__(self, chunks: list[bytes]) -> None:
        self.chunks = chunks
        self.calls: list[dict] = []

    def capabilities(self) -> TTSCaps:
        return TTSCaps(streaming=True, languages=("zh",), clone=False, emotion=True)

    async def warmup(self) -> None: ...

    def synth_stream(self, text: str, *, voice: VoiceRef, emotion: str | None):
        self.calls.append(
            {
                "text": text,
                "lang": voice.lang,
                "voice_name": voice.name,
                "emotion": emotion,
            }
        )

        async def gen():
            for c in self.chunks:
                yield c

        return gen()

    async def cancel(self, req_id: str) -> None: ...


class _EmptyTTS(_FakeTTS):
    def __init__(self) -> None:
        super().__init__([])


@contextlib.contextmanager
def _app_with(provider: TTSProvider, *, api_key: str = ""):
    app = create_app()
    with TestClient(app) as client:
        app.state.provider = provider
        app.state.settings = Settings(tts_api_key=api_key)
        app.state.ready = True
        yield client


def _pcm(samples: int, byte: int = 1) -> bytes:
    return bytes([byte, 0]) * samples


def test_speech_pcm_streams(monkeypatch) -> None:
    chunks = [_pcm(1200, 1), _pcm(800, 2)]
    fake = _FakeTTS(chunks)
    with _app_with(fake) as client:
        resp = client.post(
            "/v1/audio/speech",
            json={"model": "m", "input": "你好。", "response_format": "pcm"},
        )
        assert resp.status_code == 200
        assert resp.headers["content-type"] == "audio/pcm"
        assert resp.content == b"".join(chunks)
        assert fake.calls[0]["text"] == "你好。"


def test_speech_wav_wraps(monkeypatch) -> None:
    chunks = [_pcm(1200, 1)]
    with _app_with(_FakeTTS(chunks)) as client:
        resp = client.post(
            "/v1/audio/speech",
            json={"input": "你好。", "response_format": "wav"},
        )
        assert resp.status_code == 200
        assert resp.headers["content-type"] == "audio/wav"
        pcm, rate, ch = wav_unwrap(resp.content)
        assert (rate, ch) == (24000, 1)
        assert pcm == b"".join(chunks)


def test_speech_voice_lang_emotion_passthrough(monkeypatch) -> None:
    fake = _FakeTTS([_pcm(100)])
    with _app_with(fake) as client:
        resp = client.post(
            "/v1/audio/speech",
            json={
                "input": "hi",
                "voice": "zf_xiaoxiao",
                "lang": "zh",
                "emotion": "happy",
                "speed": 1.1,
            },
        )
        assert resp.status_code == 200
        assert fake.calls == [
            {"text": "hi", "lang": "zh", "voice_name": "zf_xiaoxiao", "emotion": "happy"}
        ]


def test_speech_validation(monkeypatch) -> None:
    with _app_with(_FakeTTS([_pcm(1)])) as client:
        r = client.post("/v1/audio/speech", json={"input": ""})
        assert r.status_code == 400
        assert r.json()["error"]["param"] == "input"

        r = client.post("/v1/audio/speech", json={"input": "x" * 4097})
        assert r.status_code == 400

        r = client.post("/v1/audio/speech", json={"input": "hi", "response_format": "mp3"})
        assert r.status_code == 400
        assert r.json()["error"]["param"] == "response_format"

        r = client.post("/v1/audio/speech", json={"input": "hi", "speed": 9})
        assert r.status_code == 400
        assert r.json()["error"]["param"] == "speed"

        r = client.post("/v1/audio/speech", content=b"not json")
        assert r.status_code == 400


def test_speech_provider_not_ready(monkeypatch) -> None:
    app = create_app()
    with TestClient(app) as client:
        app.state.provider = None
        app.state.settings = Settings()
        app.state.ready = False
        r = client.post("/v1/audio/speech", json={"input": "hi"})
        assert r.status_code == 503
        assert r.json()["error"]["type"] == "server_error"


def test_speech_empty_production_is_server_error(monkeypatch) -> None:
    with _app_with(_EmptyTTS()) as client:
        r = client.post("/v1/audio/speech", json={"input": "hi"})
        assert r.status_code == 500
        assert "no audio" in r.json()["error"]["message"]


def test_speech_auth_enforced_when_key_set(monkeypatch) -> None:
    fake = _FakeTTS([_pcm(100)])
    with _app_with(fake, api_key="k1") as client:
        r = client.post("/v1/audio/speech", json={"input": "hi"})
        assert r.status_code == 401
        assert r.json()["error"]["type"] == "authentication_error"

        r = client.post(
            "/v1/audio/speech",
            json={"input": "hi"},
            headers={"Authorization": "Bearer wrong"},
        )
        assert r.status_code == 401

        r = client.post(
            "/v1/audio/speech",
            json={"input": "hi"},
            headers={"Authorization": "Bearer k1"},
        )
        assert r.status_code == 200
