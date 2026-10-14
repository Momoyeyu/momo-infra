"""MANIFEST.toml 解析与 sha256 校验（全本地假文件，无网络/GPU）。"""

import hashlib
from pathlib import Path

import pytest

from momo_models.manifest import (
    ManifestError,
    load_manifest,
    sha256_file,
    verify_entry,
)

CONTENT_A = b"hello momo"
SHA_A = hashlib.sha256(CONTENT_A).hexdigest()

MANIFEST_OK = f"""
[[model]]
name = "fake-model"
repo_id = "owner/repo"
revision = "abc123"
dest = "owner/repo"
notes = "test"
license = {{ spdx = "apache-2.0", source = "https://example.com/license" }}
files = [
  {{ path = "a.bin", sha256 = "{SHA_A}" }},
  {{ path = "b.bin", sha256 = "TODO" }},
]
"""

# 非 SPDX 自定义许可：spdx = "other" 时必须带 name。
MANIFEST_CUSTOM_LICENSE = """
[[model]]
name = "custom-license-model"
repo_id = "owner/repo"
revision = "main"
dest = "owner/repo"
license = { spdx = "other", name = "Model License v1.1", source = "https://example.com/license" }
files = []
"""

# license.spdx = "other" 却缺 name → 非法。
MANIFEST_OTHER_NO_NAME = """
[[model]]
name = "bad-license-model"
repo_id = "owner/repo"
revision = "main"
dest = "owner/repo"
license = { spdx = "other", source = "https://example.com/license" }
files = []
"""

MANIFEST_BAD = """
[[model]]
name = "broken"
repo_id = "owner/repo"
# 缺 revision / dest
files = []
"""

# 合法但未声明 license：license 可选，缺省解析为 None。
MANIFEST_NO_LICENSE = """
[[model]]
name = "no-license"
repo_id = "owner/repo"
revision = "main"
dest = "owner/repo"
files = []
"""

# GitHub-only 上游：source = "url" 走逐文件直链，不经 HF_ENDPOINT。
MANIFEST_URL_SOURCE = f"""
[[model]]
name = "gh-only"
repo_id = "owner/repo"
revision = "master"
dest = "owner/repo"
source = "url"
files = [
]
"""

# source = "url" 但文件未给 url → 非法（pull 无从拼 URL）。
MANIFEST_URL_MISSING_URL = """
[[model]]
name = "bad-url-source"
repo_id = "owner/repo"
revision = "main"
dest = "owner/repo"
source = "url"
files = [ { path = "w.onnx", sha256 = "TODO" } ]
"""

# source 取值限定 hf|url。
MANIFEST_BAD_SOURCE = """
[[model]]
name = "bad-source"
repo_id = "owner/repo"
revision = "main"
dest = "owner/repo"
source = "ftp"
files = []
"""


def _write_manifest(tmp_path: Path, text: str = MANIFEST_OK) -> Path:
    p = tmp_path / "MANIFEST.toml"
    p.write_text(text, encoding="utf-8")
    return p


def test_load_manifest_fields(tmp_path: Path) -> None:
    entries = load_manifest(_write_manifest(tmp_path))
    assert len(entries) == 1
    e = entries[0]
    assert e.name == "fake-model"
    assert e.repo_id == "owner/repo"
    assert e.revision == "abc123"
    assert e.dest == "owner/repo"
    assert [f.path for f in e.files] == ["a.bin", "b.bin"]
    assert e.files[0].sha256 == SHA_A


def test_load_manifest_license_fields(tmp_path: Path) -> None:
    e = load_manifest(_write_manifest(tmp_path))[0]
    assert e.license is not None
    assert e.license.spdx == "apache-2.0"
    assert e.license.source == "https://example.com/license"
    assert e.license.name == ""


def test_load_manifest_license_absent_is_none(tmp_path: Path) -> None:
    """license 为可选字段：缺省时不报错，解析为 None。"""
    e = load_manifest(_write_manifest(tmp_path, MANIFEST_NO_LICENSE))[0]
    assert e.license is None


def test_load_manifest_custom_license(tmp_path: Path) -> None:
    e = load_manifest(_write_manifest(tmp_path, MANIFEST_CUSTOM_LICENSE))[0]
    assert e.license is not None
    assert e.license.spdx == "other"
    assert e.license.name == "Model License v1.1"
    assert e.license.source == "https://example.com/license"


def test_load_manifest_other_license_requires_name(tmp_path: Path) -> None:
    with pytest.raises(ManifestError):
        load_manifest(_write_manifest(tmp_path, MANIFEST_OTHER_NO_NAME))


def test_load_manifest_missing_field(tmp_path: Path) -> None:
    with pytest.raises(ManifestError):
        load_manifest(_write_manifest(tmp_path, MANIFEST_BAD))


def test_load_manifest_url_source(tmp_path: Path) -> None:
    """source = "url" 条目解析出直链字段；缺省条目 source = "hf"。"""
    e = load_manifest(_write_manifest(tmp_path, MANIFEST_URL_SOURCE))[0]
    assert e.source == "url"

    default = load_manifest(_write_manifest(tmp_path, MANIFEST_NO_LICENSE))[0]
    assert default.source == "hf"


def test_load_manifest_url_source_requires_file_url(tmp_path: Path) -> None:
    with pytest.raises(ManifestError):
        load_manifest(_write_manifest(tmp_path, MANIFEST_URL_MISSING_URL))


def test_load_manifest_rejects_unknown_source(tmp_path: Path) -> None:
    with pytest.raises(ManifestError):
        load_manifest(_write_manifest(tmp_path, MANIFEST_BAD_SOURCE))


def test_sha256_file(tmp_path: Path) -> None:
    f = tmp_path / "x.bin"
    f.write_bytes(CONTENT_A)
    assert sha256_file(f) == SHA_A


def test_verify_entry_ok_and_todo_skipped(tmp_path: Path) -> None:
    """sha256 匹配 + TODO 占位跳过内容校验 → ok。"""
    entry = load_manifest(_write_manifest(tmp_path))[0]
    root = tmp_path / "models" / "owner" / "repo"
    root.mkdir(parents=True)
    (root / "a.bin").write_bytes(CONTENT_A)
    (root / "b.bin").write_bytes(b"anything")  # TODO 占位不校验内容
    result = verify_entry(entry, tmp_path / "models")
    assert result.ok
    assert not result.missing
    assert not result.mismatched


def test_verify_entry_missing(tmp_path: Path) -> None:
    entry = load_manifest(_write_manifest(tmp_path))[0]
    result = verify_entry(entry, tmp_path / "models")
    assert not result.ok
    assert result.missing == ("a.bin", "b.bin")


def test_verify_entry_mismatch(tmp_path: Path) -> None:
    entry = load_manifest(_write_manifest(tmp_path))[0]
    root = tmp_path / "models" / "owner" / "repo"
    root.mkdir(parents=True)
    (root / "a.bin").write_bytes(b"tampered")
    (root / "b.bin").write_bytes(b"x")
    result = verify_entry(entry, tmp_path / "models")
    assert not result.ok
    assert result.mismatched == ("a.bin",)


def test_real_manifest_parses() -> None:
    """仓内 models/MANIFEST.toml 骨架本身须可被解析。"""
    from momo_models.manifest import DEFAULT_MANIFEST

    entries = load_manifest(DEFAULT_MANIFEST)
    names = {e.name for e in entries}
    assert {"kokoro-82m", "f5-tts-v1", "sense-voice-small-int8"} <= names
    for e in entries:
        assert e.repo_id and e.dest


def test_real_manifest_declares_license() -> None:
    """机读校验：仓内每条目都须声明结构化 license（spdx + source）。"""
    from momo_models.manifest import DEFAULT_MANIFEST

    entries = load_manifest(DEFAULT_MANIFEST)
    assert entries
    for e in entries:
        assert e.license is not None, f"{e.name}: missing license"
        assert e.license.spdx, f"{e.name}: missing license.spdx"
        assert e.license.source, f"{e.name}: missing license.source"

    by_name = {e.name: e.license for e in entries}
    # 逐条许可按上游核对（见 PR 证据）；这里 pin 关键值防回退。
    assert by_name["kokoro-82m"].spdx == "apache-2.0"
    assert by_name["cosyvoice3-0.5b"].spdx == "apache-2.0"
    assert by_name["f5-tts-v1"].spdx == "cc-by-nc-4.0"
    assert by_name["vocos-mel-24khz"].spdx == "mit"
    assert by_name["silero-vad"].spdx == "mit"
    # 非 SPDX 自定义许可：spdx = "other" 且带 name。
    assert by_name["sense-voice-small-int8"].spdx == "other"
    assert by_name["sense-voice-small-int8"].name


def test_real_manifest_silero_uses_url_source() -> None:
    """silero-vad 上游仅 GitHub：须声明 source = "url" + 直链，其余条目默认 hf。"""
    from momo_models.manifest import DEFAULT_MANIFEST

    entries = load_manifest(DEFAULT_MANIFEST)
    by_name = {e.name: e for e in entries}
    for e in entries:
        assert e.source in ("hf", "url")
    silero = by_name["silero-vad"]
    assert silero.source == "url"
    assert silero.files[0].url and silero.files[0].url.startswith("https://")
