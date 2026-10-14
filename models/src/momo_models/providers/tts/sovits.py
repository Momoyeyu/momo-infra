"""GPT-SoVITS（备选克隆路线）：官方标 4060Ti RTF 0.028（未实测）。

代码侵入性强，建议子进程隔离而非 import。见 momo-models.md「provider
与产物」与 training/voice/sft_sovits.md。
"""

from collections.abc import AsyncIterator

from momo_models.config import Settings
from momo_models.providers.base import TTSCaps, TTSProvider, VoiceRef

_TODO = "sovits integration: implement in P2 (subprocess isolation plan TBD)"


class SoVITSProvider(TTSProvider):
    name = "sovits"

    def __init__(self, settings: Settings) -> None:
        self._settings = settings

    def capabilities(self) -> TTSCaps:
        return TTSCaps(
            streaming=True,
            languages=("zh", "en", "ja"),
            clone=True,
            emotion=False,
        )

    async def warmup(self) -> None:
        raise NotImplementedError(_TODO)

    async def synth_stream(
        self, text: str, *, voice: VoiceRef, emotion: str | None
    ) -> AsyncIterator[bytes]:
        raise NotImplementedError(_TODO)
        yield  # pragma: no cover —— 保持 async generator 类型

    async def cancel(self, req_id: str) -> None:
        raise NotImplementedError(_TODO)
