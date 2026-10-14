"""OpenAI-compatible 薄客户端：通吃 mlx_lm.server / vLLM / DeepSeek。

**仅 eval/bench 用**——运行时经 MOMO_LLM_ENDPOINT 直连，不经本层（ADR-006）。
"""

from collections.abc import AsyncIterator

from momo_models.providers.base import ChatDelta, ChatRequest, LLMProvider

_TODO = "openai_compat integration: implement when P3 eval/bench needs it (openai.AsyncOpenAI)"


class OpenAICompatProvider(LLMProvider):
    """`base_url` 指向 MOMO_LLM_ENDPOINT；`api_key` 本地可空。"""

    name = "openai_compat"

    def __init__(self, base_url: str, api_key: str = "", model: str = "") -> None:
        self.base_url = base_url
        self.api_key = api_key
        self.model = model

    async def chat_stream(self, req: ChatRequest) -> AsyncIterator[ChatDelta]:
        raise NotImplementedError(_TODO)
        yield  # pragma: no cover —— 保持 async generator 类型

    async def list_models(self) -> list[str]:
        raise NotImplementedError(_TODO)

    async def aclose(self) -> None:
        return None
