//! 模型清单 core：`GET /api/models` 用——MANIFEST.toml 条目与
//! `$MOMO_MODELS_DIR/models.lock` 逐项 merge，得出 `missing/partial/verified`
//! 状态（`active` 按当前 env 激活选择标记；口径见 05-ops/model-fetch.md）。
//! 只做文件 stat，不复算 sha256（GB 级权重上 GET 不能做内容校验）；
//! 校验口径：lock 记录存在且 size 一致（manifest 声明 size/sha256 时同步比对，
//! 不一致记 warn）。激活写回走 #56 `/api/config` 通路（未实装，本模块只读）。

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::api::dto::{ActiveSelection, ModelFileStatus, ModelStatus, ModelsReport};
use crate::env_cfg::EnvCfg;
use crate::manifest::{is_placeholder_sha, FileEntry, Manifest, ModelEntry, ModelsLock};
use crate::output::AppError;

/// `GET /api/models` core：收集清单报告。MANIFEST 缺失不报错——
/// 返回 `manifest: null` + warn（web 页面需区分「未部署清单」与「服务故障」）。
/// 文件 stat 量小但仍是 IO，web 侧 `spawn_blocking` 调用。
pub fn collect_report() -> Result<ModelsReport, AppError> {
    let env = EnvCfg;
    let models_dir = env.models_dir();
    let active_names = active_name_set(&env);

    let mut warn = Vec::new();
    let mut models = Vec::new();
    let mut manifest_str = None;

    match crate::manifest::default_manifest_path() {
        Some(path) => {
            let manifest = Manifest::load(&path)?;
            let lock_path = models_dir.join("models.lock");
            let lock = ModelsLock::load(&lock_path)?;
            for m in &manifest.models {
                models.push(model_status(m, &lock, &models_dir, &active_names));
            }
            manifest_str = Some(path.display().to_string());
        }
        None => warn.push(format!(
            "MANIFEST.toml not found (tried {})",
            crate::manifest::manifest_candidates().join(", ")
        )),
    }

    Ok(ModelsReport {
        schema: crate::api::dto::SCHEMA_VERSION,
        manifest: manifest_str,
        models_dir: models_dir.display().to_string(),
        models,
        active: ActiveSelection {
            asr_provider: env.get("MOMO_ASR_PROVIDER"),
            asr_model: env.get("MOMO_ASR_MODEL"),
            tts_provider: env.get("MOMO_TTS_PROVIDER"),
            tts_voice: env.get("MOMO_TTS_VOICE"),
            tts_lang: env.get("MOMO_TTS_LANG"),
            brain_endpoint: env.get("MOMO_LLM_ENDPOINT"),
            brain_model: env.get("MOMO_LLM_MODEL"),
        },
        warn,
    })
}

/// 激活选择引用的 MANIFEST 条目名集合（`active` 标记用）。
fn active_name_set(env: &EnvCfg) -> BTreeSet<String> {
    ["MOMO_ASR_MODEL", "MOMO_LLM_MODEL", "MOMO_TTS_VOICE"]
        .iter()
        .filter_map(|k| env.get(k))
        .collect()
}

/// 单条目 merge：逐文件 missing/unverified/verified → 条目 missing/partial/verified。
fn model_status(
    m: &ModelEntry,
    lock: &ModelsLock,
    models_dir: &Path,
    active_names: &BTreeSet<String>,
) -> ModelStatus {
    let dir = m.dest_dir(models_dir);
    let lock_entry = lock
        .entries
        .iter()
        .find(|e| e.name == m.name || e.repo == format!("{}@{}", m.repo_id, m.revision));

    let mut warn = Vec::new();
    let mut files = Vec::new();
    let mut present = 0usize;
    let mut verified = 0usize;

    for f in &m.files {
        let (fs_state, actual, file_warn) = file_status(&dir, f, lock_entry.map(|e| &e.files[..]));
        warn.extend(file_warn);
        match fs_state {
            "verified" => {
                verified += 1;
                present += 1;
            }
            "unverified" => present += 1,
            _ => {}
        }
        files.push(ModelFileStatus {
            path: f.path.clone(),
            state: fs_state,
            expected_size: f.size,
            actual_size: actual,
            sha256_placeholder: f.sha256.as_deref().is_some_and(is_placeholder_sha),
        });
    }

    let state = if m.files.is_empty() {
        warn.push("entry declares no files".into());
        "missing"
    } else if verified == m.files.len() {
        "verified"
    } else if present == 0 {
        "missing"
    } else {
        "partial"
    };

    ModelStatus {
        name: m.name.clone(),
        repo_id: m.repo_id.clone(),
        revision: m.revision.clone(),
        dest: m.dest().display().to_string(),
        notes: m.notes.clone(),
        state,
        active: active_names.contains(&m.name),
        warn,
        files,
    }
}

/// 单文件 merge 判定：`(state, actual_size, warn)`。
/// verified = 在盘 + lock 有记录 + lock.size==实际 + manifest size（若声明）一致
/// + manifest sha256（若为非占位声明）== lock.sha256。
fn file_status(
    dir: &Path,
    f: &FileEntry,
    lock_files: Option<&[crate::manifest::LockFile]>,
) -> (&'static str, Option<u64>, Vec<String>) {
    let disk = dir.join(&f.path);
    let actual = fs::metadata(&disk)
        .ok()
        .filter(|md| md.is_file())
        .map(|md| md.len());
    let Some(sz) = actual else {
        return ("missing", None, Vec::new());
    };

    let mut warn = Vec::new();
    if f.sha256.as_deref().is_some_and(is_placeholder_sha) {
        warn.push(format!("{}: sha256 placeholder (TODO)", f.path));
    }

    let lock_file = lock_files.and_then(|files| files.iter().find(|lf| lf.path == f.path));
    if let Some(e) = f.size.filter(|e| *e != sz) {
        warn.push(format!("{}: size {sz} != expected {e}", f.path));
    }
    let ok = match lock_file {
        // 在盘但 lock 无记录（手动放入/外部下载）：无法证明校验过 → unverified
        None => false,
        Some(lf) if lf.size != sz => {
            warn.push(format!("{}: size {sz} != lock {}", f.path, lf.size));
            false
        }
        Some(lf) => match &f.sha256 {
            Some(h) if !is_placeholder_sha(h) && !lf.sha256.eq_ignore_ascii_case(h) => {
                warn.push(format!(
                    "{}: lock sha256 {} != manifest {}",
                    f.path,
                    &lf.sha256[..12.min(lf.sha256.len())],
                    &h[..12.min(h.len())]
                ));
                false
            }
            _ => true,
        },
    };
    (if ok { "verified" } else { "unverified" }, actual, warn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: path.into(),
            url: None,
            sha256: None,
            size: Some(size),
        }
    }

    /// 磁盘缺失 → missing；在盘但 lock 无记录 → unverified → 条目 partial。
    #[test]
    fn file_state_missing_vs_unverified() {
        let dir = std::env::temp_dir().join(format!("momo-models-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.bin"), b"1234").unwrap();

        let f_missing = file("no.bin", 4);
        let (s, _, _) = file_status(&dir, &f_missing, None);
        assert_eq!(s, "missing");

        let f_no_lock = file("a.bin", 4);
        let (s, sz, _) = file_status(&dir, &f_no_lock, None);
        assert_eq!(s, "unverified");
        assert_eq!(sz, Some(4));

        let _ = fs::remove_dir_all(&dir);
    }

    /// lock size 一致 → verified；不一致 → unverified + warn。
    #[test]
    fn file_state_verified_by_lock() {
        let dir = std::env::temp_dir().join(format!("momo-models-lk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.bin"), b"1234").unwrap();

        let lf = crate::manifest::LockFile {
            path: "a.bin".into(),
            size: 4,
            sha256: "x".repeat(64),
        };
        let (s, _, w) = file_status(&dir, &file("a.bin", 4), Some(std::slice::from_ref(&lf)));
        assert_eq!(s, "verified");
        assert!(w.is_empty(), "{w:?}");

        let bad = crate::manifest::LockFile { size: 99, ..lf };
        let (s, _, w) = file_status(&dir, &file("a.bin", 4), Some(&[bad]));
        assert_eq!(s, "unverified");
        assert!(!w.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
