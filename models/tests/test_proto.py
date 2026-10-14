"""proto.py：ULID 与 32B 二进制音频帧编解码 round-trip（镜像 app codec.rs）。"""

import pytest

from momo_models.proto import (
    AUDIO_HEADER_LEN,
    AUDIO_VER,
    DIR_DOWNLINK,
    DIR_UPLINK,
    FLAG_EOS,
    AudioHeader,
    FrameError,
    encode_audio_frame,
    new_id,
    ulid_from_bytes,
    ulid_new,
    ulid_to_bytes,
)


def test_ulid_roundtrip() -> None:
    text = ulid_new()
    assert len(text) == 26
    assert ulid_from_bytes(ulid_to_bytes(text)) == text


def test_ulid_known_vector() -> None:
    """ULID 规范样例：01ARZ3NDEKTSV4RRFFQ69G5FAV ↔ 固定 16B。"""
    raw = bytes.fromhex("01563E3AB5D3D6764C61EFB99302BD5B")
    assert ulid_from_bytes(raw) == "01ARZ3NDEKTSV4RRFFQ69G5FAV"
    assert ulid_to_bytes("01ARZ3NDEKTSV4RRFFQ69G5FAV") == raw
    # 小写输入同解码
    assert ulid_to_bytes("01arz3ndektsv4rrffq69g5fav") == raw


def test_ulid_to_bytes_rejects_invalid() -> None:
    with pytest.raises(ValueError):
        ulid_to_bytes("SHORT")
    with pytest.raises(ValueError):
        ulid_to_bytes("!" * 26)


def test_new_id_prefix() -> None:
    sid = new_id("st")
    assert sid.startswith("st_")
    ulid_to_bytes(sid.removeprefix("st_"))  # 合法 ULID


def _header(**kw) -> AudioHeader:
    base = dict(
        direction=DIR_DOWNLINK,
        flags=0,
        stream_id=b"\x01" * 16,
        chunk_seq=3,
        sentence_seq=2,
        sample_offset=4800,
    )
    base.update(kw)
    return AudioHeader(**base)


def test_frame_roundtrip() -> None:
    payload = b"\x11\x22" * 100
    frame = encode_audio_frame(_header(), payload)
    assert len(frame) == AUDIO_HEADER_LEN + len(payload)
    assert frame[0] == AUDIO_VER
    header, decoded = AudioHeader.decode(frame)
    assert header == _header()
    assert decoded == payload
    # 小端序字段抽查
    assert int.from_bytes(frame[20:24], "little") == 3
    assert int.from_bytes(frame[28:32], "little") == 4800


def test_frame_eos_empty_payload_ok() -> None:
    frame = encode_audio_frame(_header(flags=FLAG_EOS), b"")
    header, payload = AudioHeader.decode(frame)
    assert header.eos() and payload == b""


def test_frame_rejects_short() -> None:
    with pytest.raises(FrameError, match="32"):
        AudioHeader.decode(b"\x02" * 10)


def test_frame_rejects_bad_version() -> None:
    frame = bytearray(encode_audio_frame(_header(), b"\x00\x00"))
    frame[0] = 1
    with pytest.raises(FrameError, match="version"):
        AudioHeader.decode(bytes(frame))


def test_frame_rejects_bad_direction() -> None:
    frame = bytearray(encode_audio_frame(_header(), b"\x00\x00"))
    frame[1] = 9
    with pytest.raises(FrameError, match="direction"):
        AudioHeader.decode(bytes(frame))


def test_frame_rejects_reserved_flags() -> None:
    frame = bytearray(encode_audio_frame(_header(), b"\x00\x00"))
    frame[2:4] = (FLAG_EOS | 0x2).to_bytes(2, "little")
    with pytest.raises(FrameError, match="reserved"):
        AudioHeader.decode(bytes(frame))


def test_frame_rejects_odd_payload() -> None:
    with pytest.raises(FrameError, match="odd"):
        AudioHeader.decode(encode_audio_frame(_header(), b"\x00"))


def test_frame_rejects_empty_non_eos() -> None:
    with pytest.raises(FrameError, match="EOS"):
        AudioHeader.decode(encode_audio_frame(_header(), b""))


def test_frame_rejects_uplink_sentence_seq() -> None:
    frame = encode_audio_frame(_header(direction=DIR_UPLINK, sentence_seq=1), b"\x00\x00")
    with pytest.raises(FrameError, match="uplink"):
        AudioHeader.decode(frame)


def test_header_encode_requires_16b_stream_id() -> None:
    with pytest.raises(FrameError):
        _header(stream_id=b"short").encode()
