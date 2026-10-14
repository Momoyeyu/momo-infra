//! 解析 momo-models 仓的 `models/MANIFEST.toml`；条目口径与其 `manifest.py`
//! 一致（momo-docs `02-repos/momo-models.md`、`05-ops/model-fetch.md`）。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::output::AppError;

/// MANIFEST.toml 顶层：`[[model]]` 数组。
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(rename = "model", default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// 短名，`pull --only <name>` / `gc` 引用它。
    pub name: String,
    /// HF/ModelScope repo id（`<owner>/<repo>`）；`source = "url"` 时仍作
    /// 上游身份标识（dest 默认值 + `models.lock` 的 `repo@revision`）。
    pub repo_id: String,
    /// 分支/tag/commit；默认 `main`。
    #[serde(default = "default_revision")]
    pub revision: String,
    /// 下载源：`hf`（默认，HF resolve URL）或 `url`（逐文件直链）。
    #[serde(default)]
    pub source: SourceKind,
    /// 落盘子目录（相对 `$MOMO_MODELS_DIR`）；默认镜像 `repo_id` 布局。
    #[serde(default)]
    pub dest: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// 结构化许可信息（可选；`spdx` 为 SPDX 标识或 `other`）。
    #[serde(default)]
    pub license: Option<LicenseEntry>,
    /// `[[model.files]]`：逐项校验对象。
    #[serde(default)]
    pub files: Vec<FileEntry>,
}

/// 条目许可信息（机读）。口径与 `manifest.py` 的 `ManifestLicense` 一致。
#[derive(Debug, Clone, Deserialize)]
pub struct LicenseEntry {
    /// SPDX 标识（小写，如 `apache-2.0`、`mit`），或 `other`（非 SPDX 自定义许可）。
    pub spdx: String,
    /// 核对许可的出处 URL。
    pub source: String,
    /// 自定义许可名称；`spdx = "other"` 时必填。
    #[serde(default)]
    pub name: Option<String>,
}

/// 条目下载源（`source` 字段取值）。`hf` 走 `<endpoint>/<repo_id>/resolve/
/// <revision>/<path>`；`url` 逐文件用 `files[].url` 直链（GitHub raw、
/// media.githubusercontent、HF 镜像仓等不经 `HF_ENDPOINT` 拼接的地址）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// HF 兼容端点 resolve 拼 URL（缺省行为，向后兼容）。
    #[default]
    Hf,
    /// 逐文件直链：`files[].url` 必填，`repo_id`/`revision` 不参与 URL 拼接。
    Url,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    /// 相对 dest 的文件路径。
    pub path: String,
    /// 直链下载 URL。`source = "url"` 时必填；`hf` 条目下可作单文件覆盖，
    /// 存在即绕过 repo_id/revision 拼 URL。
    #[serde(default)]
    pub url: Option<String>,
    /// 期望 sha256（64 hex，可选；缺省只做存在性检查）。
    #[serde(default)]
    pub sha256: Option<String>,
    /// 期望字节数（可选）。
    #[serde(default)]
    pub size: Option<u64>,
}

fn default_revision() -> String {
    "main".into()
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let text = fs::read_to_string(path)
            .map_err(|e| AppError::Manifest(format!("read {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, AppError> {
        let m: Manifest = toml::from_str(text)
            .map_err(|e| AppError::Manifest(format!("parse MANIFEST.toml: {e}")))?;
        m.validate()?;
        Ok(m)
    }

    /// 结构校验：name/repo_id 非空、dest 不含 `..`、sha256 为 64 hex。
    pub fn validate(&self) -> Result<(), AppError> {
        for m in &self.models {
            if m.name.trim().is_empty() {
                return Err(AppError::Manifest("model entry with empty name".into()));
            }
            if m.repo_id.trim().is_empty() {
                return Err(AppError::Manifest(format!(
                    "model '{}': empty repo_id",
                    m.name
                )));
            }
            let dest = m.dest();
            if dest.components().any(|c| c.as_os_str() == "..") {
                return Err(AppError::Manifest(format!(
                    "model '{}': dest escapes models dir",
                    m.name
                )));
            }
            if let Some(lic) = &m.license {
                if lic.spdx.trim().is_empty() {
                    return Err(AppError::Manifest(format!(
                        "model '{}': license.spdx is required",
                        m.name
                    )));
                }
                if lic.source.trim().is_empty() {
                    return Err(AppError::Manifest(format!(
                        "model '{}': license.source is required",
                        m.name
                    )));
                }
                let named = lic.name.as_deref().is_some_and(|n| !n.trim().is_empty());
                if lic.spdx.trim() == "other" && !named {
                    return Err(AppError::Manifest(format!(
                        "model '{}': license.name is required when license.spdx = \"other\"",
                        m.name
                    )));
                }
            }
            for f in &m.files {
                if f.path.trim().is_empty() {
                    return Err(AppError::Manifest(format!(
                        "model '{}': file with empty path",
                        m.name
                    )));
                }
                match &f.url {
                    Some(u) if !is_http_url(u) => {
                        return Err(AppError::Manifest(format!(
                            "model '{}': file '{}': url must be http(s)",
                            m.name, f.path
                        )));
                    }
                    None if m.source == SourceKind::Url => {
                        return Err(AppError::Manifest(format!(
                            "model '{}': file '{}': url is required when source = \"url\"",
                            m.name, f.path
                        )));
                    }
                    _ => {}
                }
                if let Some(h) = &f.sha256 {
                    if !is_sha256_hex(h) && !is_placeholder_sha(h) {
                        return Err(AppError::Manifest(format!(
                            "model '{}': file '{}': sha256 must be 64 hex chars",
                            m.name, f.path
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

impl ModelEntry {
    /// 落盘相对路径：`dest` 缺省取 `repo_id`（`<owner>/<repo>` 镜像布局）。
    pub fn dest(&self) -> PathBuf {
        PathBuf::from(self.dest.as_deref().unwrap_or(&self.repo_id))
    }

    /// `$MOMO_MODELS_DIR` 下的绝对目录。
    pub fn dest_dir(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(self.dest())
    }
}

/// `$MOMO_MODELS_DIR/models.lock`：`pull` 完成后写入，`--check` 与 `gc` 复算。
/// 口径：`repo@revision` + 文件 size/hash（05-ops/model-fetch.md）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsLock {
    #[serde(rename = "entry", default)]
    pub entries: Vec<LockEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    /// `repo_id@revision`。
    pub repo: String,
    pub name: String,
    #[serde(default)]
    pub files: Vec<LockFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

impl ModelsLock {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .map_err(|e| AppError::Manifest(format!("read {}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| AppError::Manifest(format!("parse models.lock: {e}")))
    }

    /// 原子写：先写 `models.lock.tmp` 再 rename（同目录 rename 原子）。
    /// Windows 不允许 rename 覆盖已存在文件，先删目标。
    pub fn save(&self, path: &Path) -> Result<(), AppError> {
        let tmp = path.with_extension("tmp");
        let text = toml::to_string_pretty(self)
            .map_err(|e| AppError::Manifest(format!("serialize models.lock: {e}")))?;
        fs::write(&tmp, text)
            .map_err(|e| AppError::Manifest(format!("write {}: {e}", tmp.display())))?;
        if path.exists() {
            fs::remove_file(path)
                .map_err(|e| AppError::Manifest(format!("replace {}: {e}", path.display())))?;
        }
        fs::rename(&tmp, path)
            .map_err(|e| AppError::Manifest(format!("rename {}: {e}", tmp.display())))
    }

    /// 按 name/repo 去重后插入（重复 pull 覆盖旧条目）。
    pub fn upsert(&mut self, entry: LockEntry) {
        self.entries
            .retain(|e| e.name != entry.name && e.repo != entry.repo);
        self.entries.push(entry);
    }

    /// 删除命中谓词的条目并落盘（gc 摘 stale 条目）。
    pub fn retain(&mut self, keep: impl Fn(&LockEntry) -> bool) {
        self.entries.retain(keep);
    }
}

impl LockEntry {
    /// `repo` 中的 repo_id（`<owner>/<repo>@revision` → `<owner>/<repo>`）。
    pub fn repo_id(&self) -> &str {
        self.repo.split('@').next().unwrap_or(&self.repo)
    }
}

/// MANIFEST.toml 自动探测候选（cwd 相对路径）：momo-models 仓内、
/// monorepo 根、infra/ 子目录、momo-models 兄弟仓。pull/doctor/gc 共用。
const MANIFEST_CANDIDATES: &[&str] = &[
    "models/MANIFEST.toml",
    "models/models/MANIFEST.toml",
    "../models/models/MANIFEST.toml",
    "../momo-models/models/MANIFEST.toml",
];

/// 首个存在的候选路径；都找不到返回 None（调用方决定报错或降级）。
pub fn default_manifest_path() -> Option<PathBuf> {
    MANIFEST_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// `default_manifest_path` 的候选清单（错误提示用）。
pub fn manifest_candidates() -> &'static [&'static str] {
    MANIFEST_CANDIDATES
}

/// 文件 sha256 复算（校验用）。
pub fn sha256_file(path: &Path) -> Result<String, AppError> {
    let bytes = fs::read(path).map_err(|e| {
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        ))
    })?;
    Ok(hex_lower(&Sha256::digest(&bytes)))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 直链 URL 准入：仅 http/https scheme（防 `file://` 等本地协议进入下载路径）。
fn is_http_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("https://") || s.starts_with("http://")
}

/// MANIFEST 允许的 sha256 占位值（pull 实装前按实际文件补齐）。
/// 占位项可解析但不能通过校验：`pull --check` 逐项上报。
pub fn is_placeholder_sha(s: &str) -> bool {
    s.trim().eq_ignore_ascii_case("todo")
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
