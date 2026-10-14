"""Provider 抽象基类（签名对齐 docs/02-repos/momo-models.md「provider 与产物」）。

TTS 输出 PCM 24k mono、ASR 输入 PCM 16k mono；LLM 薄客户端仅供 eval/bench
（运行时经 MOMO_LLM_ENDPOINT 直连，不经本层）。
"""

from abc import ABC, abstractmethod
from collections.abc import AsyncIterator, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, ClassVar

EMOTION_TAGS = (
    "neutral",
    "happy",
    "sad",
    "angry",
    "surprised",
    "thinking",
    "shy",
    "sleepy",
)


# ---------- LLM（eval/bench only）----------


@dataclass
class ChatRequest:
    """OpenAI-compatible 聊天请求（评测口径子集）。"""

    model: str
    messages: list[dict[str, Any]]
    temperature: float | None = None
    max_tokens: int | None = None
    extra: dict[str, Any] = field(default_factory=dict)


@dataclass
class ChatDelta:
    """流式增量。`finish_reason` 非空表示流结束。"""

    text: str = ""
    finish_reason: str | None = None
    usage: dict[str, int] | None = None


class LLMProvider(ABC):
    """仅 eval/bench 用；运行时编排器直连大脑，不经本层。"""

    name: ClassVar[str]

    @abstractmethod
    def chat_stream(self, req: ChatRequest) -> AsyncIterator[ChatDelta]:
        """流式聊天完成；实现为 async generator。"""

    @abstractmethod
    async def list_models(self) -> list[str]: ...

    @abstractmethod
    async def aclose(self) -> None: ...


# ---------- TTS ----------


@dataclass(frozen=True)
class TTSCaps:
    """能力声明：服务层据此决定可否流式/克隆/带情感。"""

    streaming: bool
    languages: tuple[str, ...]
    clone: bool
    emotion: bool


@dataclass(frozen=True)
class VoiceRef:
    """克隆参考声线。`ref_audio` 为空表示使用后端默认音色。
    `name` 为请求级音色名（/v1/audio/speech 的 `voice` 字段），
    非空时优先于 `MOMO_TTS_VOICE`；clone provider 忽略。"""

    ref_audio: Path | None = None
    ref_text: str = ""
    lang: str = "auto"
    name: str = ""


class TTSProvider(ABC):
    name: ClassVar[str]

    @abstractmethod
    def capabilities(self) -> TTSCaps:
        """能力集：streaming/languages/clone/emotion。"""

    @abstractmethod
    async def warmup(self) -> None:
        """加载模型并就绪；readyz 以此为据。"""

    @abstractmethod
    def synth_stream(
        self, text: str, *, voice: VoiceRef, emotion: str | None
    ) -> AsyncIterator[bytes]:
        """逐块产出 PCM s16le 24kHz mono；实现为 async generator。"""

    @abstractmethod
    async def cancel(self, req_id: str) -> None:
        """尽力取消在途合成（200ms 内停止产出，协议 §8 control.cancel）。"""


# ---------- ASR ----------


@dataclass(frozen=True)
class Transcript:
    """一个 VAD 段的转写结果（对应协议 input.audio.segment）。"""

    text: str
    lang: str
    duration_ms: int
    speech_emotion: str | None = None
    events: tuple[str, ...] = ()


class ASRProvider(ABC):
    name: ClassVar[str]

    @abstractmethod
    async def load(self) -> None:
        """加载模型；readyz 以此为据。"""

    @abstractmethod
    async def transcribe(self, pcm: bytes, *, lang: str | None) -> Transcript:
        """转写一段 PCM s16le 16kHz mono。"""


# ---------- VAD ----------

SPEECH_START = "speech_start"
SPEECH_END = "speech_end"


@dataclass(frozen=True)
class VadEvent:
    """VAD 边界事件；`pcm` 仅 speech_end 携带（段 PCM s16le 16k mono 供 ASR）。"""

    event: str
    at_ms: int
    segment_index: int
    pcm: bytes = b""


class VADProvider(ABC):
    """16kHz mono PCM 流 → 断句边界 + 语音段；只做断句不做识别。"""

    name: ClassVar[str]

    @abstractmethod
    async def load(self) -> None:
        """加载模型；readyz 以此为据。"""

    @abstractmethod
    def feed(self, pcm: bytes) -> Iterator[VadEvent]:
        """喂入一块 PCM s16le 16kHz mono，产出 0..n 个边界事件。"""

    @abstractmethod
    def flush(self) -> Iterator[VadEvent]:
        """capture 结束时冲刷：残留未满一窗的样本按末尾静音处理。"""

    @abstractmethod
    def reset(self) -> None:
        """丢弃流内状态（新 capture 前调用）。"""
