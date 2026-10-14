"""momo-ws v2 编解码：ULID + 32B 音频帧头；帧布局见 protocol.md「32-byte 二进制帧」。"""

import secrets
import time
from dataclasses import dataclass

AUDIO_HEADER_LEN = 32
AUDIO_VER = 2
DIR_UPLINK = 1
DIR_DOWNLINK = 2
FLAG_EOS = 0x0001
_KNOWN_FLAGS = FLAG_EOS  # reserved bits 非零即拒绝

# ---------- ULID（128-bit ↔ Crockford base32 26 字符）----------

_CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
_CROCKFORD_REV = {c: i for i, c in enumerate(_CROCKFORD)}


def ulid_new() -> str:
    """生成新 ULID（48-bit ms 时间戳 + 80-bit 随机），返回 26 字符文本。"""
    ts = int(time.time() * 1000) & ((1 << 48) - 1)
    value = (ts << 80) | int.from_bytes(secrets.token_bytes(10), "big")
    out = []
    for _ in range(26):
        out.append(_CROCKFORD[value & 0x1F])
        value >>= 5
    return "".join(reversed(out))


def ulid_to_bytes(text: str) -> bytes:
    """ULID 文本 → 16B 原始值；非法文本抛 ValueError。"""
    if len(text) != 26:
        raise ValueError(f"invalid ULID length: {text!r}")
    value = 0
    for ch in text.upper():
        digit = _CROCKFORD_REV.get(ch)
        if digit is None:
            raise ValueError(f"invalid ULID char {ch!r} in {text!r}")
        value = (value << 5) | digit
    if value >= 1 << 128:
        raise ValueError(f"ULID overflow: {text!r}")
    return value.to_bytes(16, "big")


def ulid_from_bytes(raw: bytes) -> str:
    """16B 原始值 → ULID 文本。"""
    if len(raw) != 16:
        raise ValueError(f"ULID raw must be 16 bytes, got {len(raw)}")
    value = int.from_bytes(raw, "big")
    out = []
    for _ in range(26):
        out.append(_CROCKFORD[value & 0x1F])
        value >>= 5
    return "".join(reversed(out))


def new_id(prefix: str) -> str:
    """`<prefix>_<ULID>`（`sess_`/`st_`/`t_` 等），对齐 app 侧 new_id。"""
    return f"{prefix}_{ulid_new()}"


# ---------- 二进制音频帧 ----------


class FrameError(ValueError):
    """音频帧头/载荷非法（对应 app 侧 AppError::BadMessage）。"""


@dataclass(frozen=True)
class AudioHeader:
    """32B 帧头（payload 不含在内）。"""

    direction: int  # DIR_UPLINK | DIR_DOWNLINK
    flags: int  # bit0=EOS；reserved 非零拒绝
    stream_id: bytes  # 16B ULID 原始值
    chunk_seq: int
    sentence_seq: int  # uplink 固定 0
    sample_offset: int

    def eos(self) -> bool:
        return bool(self.flags & FLAG_EOS)

    def encode(self) -> bytes:
        """编码为 32B 头（ver 自动置 2）。"""
        if len(self.stream_id) != 16:
            raise FrameError(f"stream_id must be 16 raw bytes, got {len(self.stream_id)}")
        buf = bytearray(AUDIO_HEADER_LEN)
        buf[0] = AUDIO_VER
        buf[1] = self.direction
        buf[2:4] = self.flags.to_bytes(2, "little")
        buf[4:20] = self.stream_id
        buf[20:24] = self.chunk_seq.to_bytes(4, "little")
        buf[24:28] = self.sentence_seq.to_bytes(4, "little")
        buf[28:32] = self.sample_offset.to_bytes(4, "little")
        return bytes(buf)

    @staticmethod
    def decode(frame: bytes) -> tuple["AudioHeader", bytes]:
        """解码头并返回 (header, payload)；非法帧抛 FrameError。

        校验口径同 app 侧 codec.rs；依赖 stream 状态的检查（seq gap
        等）由上层负责。
        """
        if len(frame) < AUDIO_HEADER_LEN:
            raise FrameError("header shorter than 32 bytes")
        if frame[0] != AUDIO_VER:
            raise FrameError("unsupported version")
        direction = frame[1]
        if direction not in (DIR_UPLINK, DIR_DOWNLINK):
            raise FrameError("invalid direction")
        flags = int.from_bytes(frame[2:4], "little")
        if flags & ~_KNOWN_FLAGS:
            raise FrameError("reserved flags set")
        header = AudioHeader(
            direction=direction,
            flags=flags,
            stream_id=frame[4:20],
            chunk_seq=int.from_bytes(frame[20:24], "little"),
            sentence_seq=int.from_bytes(frame[24:28], "little"),
            sample_offset=int.from_bytes(frame[28:32], "little"),
        )
        payload = frame[AUDIO_HEADER_LEN:]
        if len(payload) % 2:
            raise FrameError("odd payload length (pcm_s16le)")
        if not payload and not header.eos():
            raise FrameError("empty payload without EOS")
        if direction == DIR_UPLINK and header.sentence_seq != 0:
            raise FrameError("uplink sentence_seq must be 0")
        return header, payload


def encode_audio_frame(header: AudioHeader, payload: bytes) -> bytes:
    """`header + payload` 拼成完整二进制帧。"""
    return header.encode() + payload
