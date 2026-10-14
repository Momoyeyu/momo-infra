"""emotion tag → TTS provider 合成参数映射层（protocol.md 固定 8 tag）。

每个 provider 一张 tag→参数表，集中在此便于单测：

- `kokoro`：`speed`（KPipeline 唯一情感相关旋钮）
- `edge`：`rate`/`pitch`/`volume`（edge-tts `Communicate` 入参）
- `cosyvoice3`：`instruct` 文本指令（provider 实装时消费）
- `sovits`：`ref_emotion` 参考音情感迁移提示（provider 实装时消费）
- `f5`：无消费参数（`caps.emotion=False` → 安全忽略）

`map_emotion`：`None`/未知 tag 兜底 `neutral`；`caps.emotion=False`
或无表 provider 返回 `{}`（安全忽略不报错，产出与 neutral 一致）。
"""

from typing import Any

from momo_models.providers.base import EMOTION_TAGS, TTSCaps

NEUTRAL = "neutral"

# provider → tag → 参数表；8 tag 全覆盖，值即 provider 各自入参口径。
EMOTION_PARAMS: dict[str, dict[str, dict[str, Any]]] = {
    "kokoro": {
        "neutral": {"speed": 1.0},
        "happy": {"speed": 1.08},
        "sad": {"speed": 0.9},
        "angry": {"speed": 1.12},
        "surprised": {"speed": 1.15},
        "thinking": {"speed": 0.95},
        "shy": {"speed": 0.92},
        "sleepy": {"speed": 0.8},
    },
    "edge": {
        "neutral": {"rate": "+0%", "pitch": "+0Hz", "volume": "+0%"},
        "happy": {"rate": "+8%", "pitch": "+4Hz", "volume": "+0%"},
        "sad": {"rate": "-10%", "pitch": "-4Hz", "volume": "-10%"},
        "angry": {"rate": "+12%", "pitch": "+2Hz", "volume": "+15%"},
        "surprised": {"rate": "+15%", "pitch": "+6Hz", "volume": "+0%"},
        "thinking": {"rate": "-5%", "pitch": "-2Hz", "volume": "+0%"},
        "shy": {"rate": "-8%", "pitch": "-2Hz", "volume": "-20%"},
        "sleepy": {"rate": "-18%", "pitch": "-6Hz", "volume": "-10%"},
    },
    "cosyvoice3": {
        # instruct 文本指令模板；空串 = 不带指令（中性默认）。
        "neutral": {"instruct": ""},
        "happy": {"instruct": "很开心地说"},
        "sad": {"instruct": "很伤心地说"},
        "angry": {"instruct": "很生气地说"},
        "surprised": {"instruct": "很惊讶地说"},
        "thinking": {"instruct": "若有所思地说"},
        "shy": {"instruct": "害羞地说"},
        "sleepy": {"instruct": "困倦地说"},
    },
    "sovits": {
        # 参考音情感迁移提示：选对应情感的参考音片段做迁移。
        "neutral": {"ref_emotion": "neutral"},
        "happy": {"ref_emotion": "happy"},
        "sad": {"ref_emotion": "sad"},
        "angry": {"ref_emotion": "angry"},
        "surprised": {"ref_emotion": "surprised"},
        "thinking": {"ref_emotion": "calm"},
        "shy": {"ref_emotion": "soft"},
        "sleepy": {"ref_emotion": "tired"},
    },
}


def normalize_emotion(tag: str | None) -> str:
    """`None`/未知 tag → `neutral`；合法 8 tag 原样返回。"""
    if tag is not None and tag in EMOTION_TAGS:
        return tag
    return NEUTRAL


def map_emotion(tag: str | None, provider: str, caps: TTSCaps) -> dict[str, Any]:
    """emotion tag → provider 合成参数。

    - `caps.emotion=False` → `{}`（该 provider 产出与 neutral 一致，不报错）
    - 无参数表的 provider → `{}`
    - `None`/未知 tag → `neutral` 行
    """
    if not caps.emotion:
        return {}
    table = EMOTION_PARAMS.get(provider)
    if not table:
        return {}
    return dict(table.get(normalize_emotion(tag), table[NEUTRAL]))
