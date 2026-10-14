"""models/MANIFEST.toml 解析与 sha256 校验。

条目口径见 docs/02-repos/momo-models.md「provider 与产物」；下载由
momo-infra `momoctl pull` 执行，启动校验失败 → readyz 拒绝就绪。
"""

import hashlib
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_MANIFEST = Path(__file__).resolve().parents[2] / "models" / "MANIFEST.toml"

_SHA256_CHUNK = 1 << 20  # 1 MiB


@dataclass(frozen=True)
class ManifestFile:
    path: str
    sha256: str
    size: int | None = None
    url: str | None = None  # 直链下载地址；条目 source="url" 时必填


@dataclass(frozen=True)
class ManifestLicense:
    """条目许可信息（机读）。

    `spdx` 为 SPDX 标识（小写，如 `apache-2.0`、`mit`），或 `other` 表示
    非 SPDX 的自定义许可——此时 `name` 必填。`source` 为核对许可的出处 URL。
    """

    spdx: str
    source: str
    name: str = ""


@dataclass(frozen=True)
class ManifestEntry:
    name: str
    repo_id: str
    revision: str
    dest: str
    files: tuple[ManifestFile, ...] = field(default_factory=tuple)
    notes: str = ""
    license: ManifestLicense | None = None
    # 下载源："hf"（缺省，HF resolve 拼 URL）或 "url"（files[].url 直链）。
    # 运行时只做本地校验不下载，字段仅作口径对齐与结构校验。
    source: str = "hf"


_VALID_SOURCES = ("hf", "url")


@dataclass(frozen=True)
class VerifyResult:
    """单条目校验结果。`ok` 为真表示全部文件存在且 sha256 一致。"""

    entry: str
    ok: bool
    missing: tuple[str, ...] = ()
    mismatched: tuple[str, ...] = ()


class ManifestError(ValueError):
    """MANIFEST.toml 结构非法。"""


def sha256_file(path: Path) -> str:
    """流式计算文件 sha256，返回小写 hex。"""
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(_SHA256_CHUNK), b""):
            h.update(chunk)
    return h.hexdigest()


def _parse_license(where: str, raw: object) -> ManifestLicense | None:
    """解析可选的 `license` 表；结构非法抛 ManifestError（面向用户，英文）。"""
    if raw is None:
        return None
    if not isinstance(raw, dict):
        raise ManifestError(f"{where}: license must be a table")
    lic = ManifestLicense(
        spdx=str(raw.get("spdx", "")).strip(),
        source=str(raw.get("source", "")).strip(),
        name=str(raw.get("name", "")).strip(),
    )
    if not lic.spdx:
        raise ManifestError(f"{where}: license.spdx is required")
    if not lic.source:
        raise ManifestError(f"{where}: license.source is required")
    if lic.spdx == "other" and not lic.name:
        raise ManifestError(f'{where}: license.name is required when license.spdx = "other"')
    return lic


def load_manifest(path: Path = DEFAULT_MANIFEST) -> list[ManifestEntry]:
    """解析 MANIFEST.toml；结构非法抛 ManifestError。"""
    raw = tomllib.loads(path.read_text(encoding="utf-8"))
    entries = []
    for i, item in enumerate(raw.get("model", [])):
        where = f"model[{i}]({item.get('name', '?')})"
        try:
            files = tuple(
                ManifestFile(
                    path=str(f["path"]),
                    sha256=str(f["sha256"]).lower(),
                    size=int(f["size"]) if "size" in f else None,
                    url=str(f["url"]) if "url" in f else None,
                )
                for f in item.get("files", [])
            )
            source = str(item.get("source", "hf")).strip().lower()
            entries.append(
                ManifestEntry(
                    name=str(item["name"]),
                    repo_id=str(item["repo_id"]),
                    revision=str(item["revision"]),
                    dest=str(item["dest"]),
                    files=files,
                    notes=str(item.get("notes", "")),
                    license=_parse_license(where, item.get("license")),
                    source=source,
                )
            )
        except KeyError as e:
            raise ManifestError(f"{where}: missing required fields {e}") from e
        if source not in _VALID_SOURCES:
            raise ManifestError(f"{where}: source must be one of {_VALID_SOURCES}")
        if source == "url":
            for f in entries[-1].files:
                if not f.url or not f.url.startswith(("http://", "https://")):
                    raise ManifestError(
                        f'{where}: file "{f.path}" needs an http(s) url when source = "url"'
                    )
    return entries


def verify_entry(entry: ManifestEntry, models_dir: Path) -> VerifyResult:
    """按 sha256 校验 `models_dir/entry.dest/` 下全部声明文件。

    sha256 为 "TODO" 占位的文件跳过内容校验（清单未补齐阶段不阻塞），
    但文件缺失仍计入 missing。
    """
    missing: list[str] = []
    mismatched: list[str] = []
    root = models_dir / entry.dest
    for f in entry.files:
        fp = root / f.path
        if not fp.is_file():
            missing.append(f.path)
            continue
        if f.sha256 == "todo":
            continue
        if sha256_file(fp) != f.sha256:
            mismatched.append(f.path)
    return VerifyResult(
        entry=entry.name,
        ok=not missing and not mismatched,
        missing=tuple(missing),
        mismatched=tuple(mismatched),
    )


def verify_all(models_dir: Path, path: Path = DEFAULT_MANIFEST) -> list[VerifyResult]:
    """逐条目校验；供服务启动时 readyz 门控与脚本复用。"""
    return [verify_entry(e, models_dir) for e in load_manifest(path)]
