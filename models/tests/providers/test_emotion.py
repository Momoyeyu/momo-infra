"""emotion 映射层单测：8 tag 全覆盖、兜底与 caps.emotion=False 安全忽略。"""

import pytest

from momo_models.providers.base import EMOTION_TAGS, TTSCaps
from momo_models.providers.tts.emotion import (
    EMOTION_PARAMS,
    map_emotion,
    normalize_emotion,
)

_CAPS_EMOTION = TTSCaps(streaming=True, languages=("zh",), clone=False, emotion=True)
_CAPS_NO_EMOTION = TTSCaps(streaming=True, languages=("zh",), clone=False, emotion=False)


def test_tables_cover_all_tags() -> None:
    """每张 provider 参数表覆盖协议固定 8 tag。"""
    for provider, table in EMOTION_PARAMS.items():
        assert set(table) == set(EMOTION_TAGS), f"{provider} 缺 tag"


def test_normalize_emotion() -> None:
    assert normalize_emotion(None) == "neutral"
    assert normalize_emotion("happy") == "happy"
    assert normalize_emotion("nope") == "neutral"
    assert normalize_emotion("") == "neutral"


def test_map_emotion_kokoro_speed() -> None:
    """kokoro：tag → speed，8 tag 全覆盖且互有区分。"""
    speeds = {tag: map_emotion(tag, "kokoro", _CAPS_EMOTION)["speed"] for tag in EMOTION_TAGS}
    assert speeds["neutral"] == 1.0
    assert speeds["happy"] > 1.0
    assert speeds["sad"] < 1.0
    assert speeds["sleepy"] < speeds["sad"]
    assert len(set(speeds.values())) == 8


def test_map_emotion_edge_params() -> None:
    """edge：tag → rate/pitch/volume（edge-tts Communicate 入参口径）。"""
    params = map_emotion("happy", "edge", _CAPS_EMOTION)
    assert set(params) == {"rate", "pitch", "volume"}
    assert params["rate"].startswith("+")
    assert map_emotion("neutral", "edge", _CAPS_EMOTION) == {
        "rate": "+0%",
        "pitch": "+0Hz",
        "volume": "+0%",
    }


def test_map_emotion_cosyvoice3_instruct() -> None:
    """cosyvoice3：tag → instruct 文本指令；neutral 为空指令。"""
    assert map_emotion("happy", "cosyvoice3", _CAPS_EMOTION) == {"instruct": "很开心地说"}
    assert map_emotion("neutral", "cosyvoice3", _CAPS_EMOTION) == {"instruct": ""}


def test_map_emotion_sovits_ref_hint() -> None:
    """sovits：tag → 参考音情感迁移提示。"""
    assert map_emotion("angry", "sovits", _CAPS_EMOTION) == {"ref_emotion": "angry"}


def test_map_emotion_none_and_unknown_tag_fall_back_to_neutral() -> None:
    """None/未知 tag → neutral 行（#33 兜底口径）。"""
    neutral = map_emotion("neutral", "kokoro", _CAPS_EMOTION)
    assert map_emotion(None, "kokoro", _CAPS_EMOTION) == neutral
    assert map_emotion("excited", "kokoro", _CAPS_EMOTION) == neutral


@pytest.mark.parametrize("provider", ["kokoro", "edge", "cosyvoice3", "sovits", "f5"])
def test_map_emotion_caps_false_returns_empty(provider: str) -> None:
    """caps.emotion=False → {}：任意 tag 产出与 neutral 一致，不报错。"""
    caps = _CAPS_NO_EMOTION
    for tag in (*EMOTION_TAGS, None, "nope"):
        assert map_emotion(tag, provider, caps) == {}


def test_map_emotion_unknown_provider() -> None:
    assert map_emotion("happy", "no-such-provider", _CAPS_EMOTION) == {}
