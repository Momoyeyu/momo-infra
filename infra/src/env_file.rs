//! `.env` / `.env.<profile>` 文件机制：定位、解析、保序写回、原子替换、备份回滚。
//! `momoctl switch` 与 web 控制台配置页（`/api/config`）共用同一套 core，
//! 保证「写回 `.env.<profile>`」的口径只有一份（momo-infra.md Web 控制台节）。
//!
//! 写路径约束：只允许写 `.env` 同目录下的 `<link_name>.<profile>`
//! （profile 白名单 normal|degraded，文件名不含分隔符，无路径穿越面）。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::api::dto::DiffEntry;
use crate::config_schema::{self, KeySpec, Schema};
use crate::env_cfg;
use crate::output::AppError;
use crate::serve::proc;

/// 可写回的档位白名单（文件名后缀与 `momoctl switch` 生成口径一致）。
pub const PROFILES: &[&str] = &["normal", "degraded"];

/// `.env` 定位：`--env-file` > dotenvy 自动发现 > `./.env`。
pub fn resolve_env_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(p) = dotenvy::dotenv() {
        return p;
    }
    PathBuf::from(".env")
}

/// `.env` 若是符号链接返回其目标路径（相对则拼同目录）；普通文件返回自身；缺失 None。
pub fn read_link_target(link: &Path) -> Option<PathBuf> {
    match fs::symlink_metadata(link) {
        Ok(md) if md.file_type().is_symlink() => {
            let t = fs::read_link(link).ok()?;
            Some(if t.is_absolute() {
                t
            } else {
                link.parent().unwrap_or(Path::new(".")).join(t)
            })
        }
        Ok(_) => Some(link.to_path_buf()),
        Err(_) => None,
    }
}

/// 解析 env 文件为 key→value（dotenvy 口径；解析失败上抛 config 错误）。
pub fn parse_env_file(path: &Path) -> Result<BTreeMap<String, String>, AppError> {
    let iter = dotenvy::from_path_iter(path)
        .map_err(|e| AppError::Config(format!("read {}: {e}", path.display())))?;
    let mut out = BTreeMap::new();
    for pair in iter {
        let (k, v) =
            pair.map_err(|e| AppError::Config(format!("parse {}: {e}", path.display())))?;
        out.insert(k, v);
    }
    Ok(out)
}

/// `.env.<profile>` 目标路径：`<link_name>.<profile>` 落在 `.env` 同目录。
/// profile 白名单（normal|degraded）+ 固定文件名拼接——不接纳任何路径片段，
/// resolve 结果必然位于 env 目录内（无 `..`/绝对路径注入面）。
pub fn profile_env_path(env_link: &Path, profile: &str) -> Result<PathBuf, AppError> {
    if !PROFILES.contains(&profile) {
        return Err(AppError::Config(format!(
            "invalid profile '{profile}' (expected normal|degraded)"
        )));
    }
    let link_name = env_link
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".env".into());
    let file_name = format!("{link_name}.{profile}");
    // 双保险：白名单已限定 file_name 为固定字面量，仍拒绝任何分隔符/穿越片段。
    if file_name != file_name.replace(['/', '\\'], "") || file_name.contains("..") {
        return Err(AppError::Config(format!(
            "refusing env file name '{file_name}' (path traversal rejected)"
        )));
    }
    let dir = env_link
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(dir.join(file_name))
}

/// `<path>.bak` 备份路径（`.env.normal` → `.env.normal.bak`）。
pub fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".env".into());
    path.with_file_name(format!("{name}.bak"))
}

/// 活跃档位：`active_profile` 状态文件（serve/switch 写）> `MOMO_PROFILE` > normal。
pub fn active_profile() -> String {
    let from_state = fs::read_to_string(proc::active_profile_file())
        .ok()
        .map(|s| s.trim().to_string());
    let from_env = env_cfg::EnvCfg.get("MOMO_PROFILE");
    for cand in [from_state, from_env].into_iter().flatten() {
        if PROFILES.contains(&cand.as_str()) {
            return cand;
        }
    }
    "normal".into()
}

/// `config.schema.toml` 加载：env 目录向上 `find_root` 命中则按盘加载；
/// 仓库外运行（找不到 schema 文件）回退编译期内嵌的仓库根 schema。
pub fn load_schema(env_dir: &Path) -> Result<Schema, AppError> {
    if let Some(root) = config_schema::find_root(env_dir) {
        return Schema::load(&root.join("config.schema.toml"));
    }
    Schema::parse(include_str!("../../config.schema.toml"))
}

// ---------- 敏感键与打码（config.md §4） ----------

/// `*_API_KEY/*_TOKEN/*_SECRET` 后缀兜底打码（schema 缺失时的保守判据）。
pub fn is_sensitive_name(key: &str) -> bool {
    ["_API_KEY", "_TOKEN", "_SECRET"]
        .iter()
        .any(|suf| key.ends_with(suf))
}

/// 敏感键集合：schema `sensitive` 标记 ∪ `env_cfg::SENSITIVE_KEYS`。
/// 调用方再按需叠加 `is_sensitive_name` 兜底（env_diff 内部已叠）。
pub fn sensitive_keys(schema: Option<&Schema>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = env_cfg::SENSITIVE_KEYS
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(s) = schema {
        out.extend(s.key.iter().filter(|k| k.sensitive).map(|k| k.name.clone()));
    }
    out
}

/// 打码（config.md §4：显示前 4 位 + `…`）。
pub fn mask(v: &str) -> String {
    if v.is_empty() {
        return "(empty)".into();
    }
    if v.len() <= 4 {
        return "…".into();
    }
    format!("{}…", &v[..4])
}

/// 新旧 env 差异：并集键序，逐键 from→to；敏感键打码。
pub fn env_diff(
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
    sensitive: &BTreeSet<String>,
) -> Vec<DiffEntry> {
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    let mut out = Vec::new();
    for k in keys {
        let (a, b) = (old.get(k), new.get(k));
        if a == b {
            continue;
        }
        let hide = sensitive.contains(k) || is_sensitive_name(k);
        out.push(DiffEntry {
            key: k.clone(),
            from: a.map(|v| if hide { mask(v) } else { v.clone() }),
            to: b.map(|v| if hide { mask(v) } else { v.clone() }),
        });
    }
    out
}

// ---------- 写回：值域校验 + 保序重写 + 原子替换 + 备份 ----------

/// schema 值域校验（`PUT /api/config` 与前端即时校验共用口径）：
/// 空值仅 `string`/`path` 允许（留空占位键）；`enum` 须 ∈ `values`；
/// `bool` 须 true/false；`int` 须可解析 i64；`url` 须带 scheme（含 ws://）。
/// 未知 type 按 string 放行（schema 演进时旧 server 不拒新键型）。
pub fn validate_value(spec: &KeySpec, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return match spec.kind.as_str() {
            "string" | "path" => Ok(()),
            other => Err(format!(
                "value must not be empty (type {other} requires a value)"
            )),
        };
    }
    match spec.kind.as_str() {
        "enum" if !spec.values.iter().any(|x| x == value) => {
            Err(format!("'{value}' not in allowed values {:?}", spec.values))
        }
        "bool" if value != "true" && value != "false" => {
            Err(format!("'{value}' is not a bool (expected true|false)"))
        }
        "int" if value.parse::<i64>().is_err() => Err(format!("'{value}' is not an int")),
        "url" if !value.contains("://") => Err(format!(
            "'{value}' is missing a URL scheme (e.g. http:// or ws://)"
        )),
        _ => Ok(()),
    }
}

/// 保注释/保序写回：逐行扫描，命中的 `KEY=` 行替换值并保留行尾注释与
/// 行首缩进/`export ` 前缀；文件中未出现的待写键按字典序追加到文件尾。
/// 幂等：同一 changes 重复应用结果相同。
pub fn apply_changes(text: &str, changes: &BTreeMap<String, String>) -> String {
    let mut pending = changes.clone();
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        match split_kv_line(line) {
            Some((key, prefix, comment)) if pending.contains_key(key) => {
                let value = pending.remove(key).expect("checked contains_key");
                out.push_str(&prefix);
                out.push_str(&format_value(&value));
                out.push_str(&comment);
                out.push('\n');
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    if !pending.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        for (k, v) in &pending {
            out.push_str(k);
            out.push('=');
            out.push_str(&format_value(v));
            out.push('\n');
        }
    }
    out
}

/// 拆 `KEY=value  # comment` 行 → (key, 行首段(含 `KEY=` 与前缀), 行尾注释段)。
/// 注释/空行/无 `=` 行返回 None。注释界定：`#` 前是空白（或值域为空），
/// 与 `.env.example` 的书写口径一致。
fn split_kv_line(line: &str) -> Option<(&str, String, String)> {
    let indent_len = line.len() - line.trim_start().len();
    let body = &line[indent_len..];
    if body.is_empty() || body.starts_with('#') {
        return None;
    }
    let body = body.strip_prefix("export ").unwrap_or(body);
    let eq = body.find('=')?;
    let key = body[..eq].trim_end();
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    // prefix 保留行首缩进与原样的 `export `。
    let prefix = {
        let mut p = String::with_capacity(eq + 8 + indent_len);
        p.push_str(&line[..indent_len]);
        if line[indent_len..].starts_with("export ") {
            p.push_str("export ");
        }
        p.push_str(key);
        p.push('=');
        p
    };
    let rest = &body[eq + 1..];
    // 注释起点：首个「前一字符为空白」的 `#`（`KEY= # c` 这类值域为空的也认）。
    // 引号开头的值不查行尾注释（`"a # b"` 内的 # 不是注释，反正值整体被替换）。
    let comment_at = if rest.trim_start().starts_with(['"', '\'']) {
        None
    } else {
        rest.char_indices().find_map(|(i, c)| {
            if c == '#' && i > 0 && rest.as_bytes()[i - 1].is_ascii_whitespace() {
                Some(i)
            } else {
                None
            }
        })
    };
    let comment = match comment_at {
        // 连同 `#` 前的空白一起原样保留（作为新行的行尾注释段）。
        Some(i) => rest[rest[..i].trim_end().len()..].to_string(),
        None => String::new(),
    };
    Some((key, prefix, comment))
}

/// dotenv 值转义：含空白/`#`/引号/换行的值用双引号包裹（`\` `"` `\n` `\r` 转义），
/// 其余按裸值直写；空值写 `KEY=`。
fn format_value(value: &str) -> String {
    let needs_quote = value.is_empty()
        || value
            .bytes()
            .any(|b| matches!(b, b' ' | b'\t' | b'#' | b'"' | b'\'' | b'\n' | b'\r'))
        || value != value.trim();
    if !needs_quote {
        return value.to_string();
    }
    if value.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 同目录临时文件 + `rename` 原子替换：写入窗口内目标要么旧要么新；
/// tmp 与目标同目录保证不跨文件系统。失败清理 tmp，目标保持原样。
pub fn atomic_write(path: &Path, content: &str) -> Result<(), AppError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".env".into());
    let tmp = path.with_file_name(format!(".{name}.momoctl-{}.tmp", std::process::id()));
    if let Err(e) = fs::write(&tmp, content) {
        let _ = fs::remove_file(&tmp);
        return Err(AppError::Io(std::io::Error::new(
            e.kind(),
            format!("write {}: {e}", tmp.display()),
        )));
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!("rename {} -> {}: {e}", tmp.display(), path.display()),
        ))
    })
}

/// 写前备份：copy `path` → `<path>.bak`（覆盖旧 bak，rollback 用最新一份）。
/// 源文件缺失 → Config 错误（不生成空备份）。
pub fn backup_file(path: &Path) -> Result<PathBuf, AppError> {
    if !path.is_file() {
        return Err(AppError::Config(format!(
            "{} not found; run `momoctl switch --to <profile>` to create it",
            path.display()
        )));
    }
    let bak = backup_path(path);
    fs::copy(path, &bak).map_err(|e| {
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!("backup {} -> {}: {e}", path.display(), bak.display()),
        ))
    })?;
    Ok(bak)
}

/// 从 `<path>.bak` 原子恢复（tmp + rename；bak 本身保留，可重复回滚）。
/// bak 缺失 → Config 错误。
pub fn restore_backup(path: &Path) -> Result<PathBuf, AppError> {
    let bak = backup_path(path);
    if !bak.is_file() {
        return Err(AppError::Config(format!(
            "no backup {}; nothing to roll back",
            bak.display()
        )));
    }
    let text = fs::read_to_string(&bak).map_err(|e| {
        AppError::Io(std::io::Error::new(
            e.kind(),
            format!("read {}: {e}", bak.display()),
        ))
    })?;
    atomic_write(path, &text)?;
    Ok(bak)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 临时目录：test-pid-标签 唯一。
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "momoctl-envfile-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// profile 白名单：normal/degraded 生成 `<link>.<profile>`；其余一律拒绝。
    #[test]
    fn profile_env_path_whitelist() {
        let link = Path::new("/repo/.env");
        assert_eq!(
            profile_env_path(link, "normal").unwrap(),
            Path::new("/repo/.env.normal")
        );
        assert!(profile_env_path(link, "degraded").is_ok());
        for bad in ["", "prod", "../x", ".", "NORMAL", "normal/x", "nor mal"] {
            assert!(profile_env_path(link, bad).is_err(), "{bad}");
        }
        // 非 .env 命名的 link（--env-file foo.env）同样按 <name>.<profile> 派生。
        assert_eq!(
            profile_env_path(Path::new("/d/foo.env"), "normal").unwrap(),
            Path::new("/d/foo.env.normal")
        );
    }

    /// apply_changes：命中行换值且保留行尾注释/缩进/export 前缀；未命中键追加尾部。
    #[test]
    fn apply_changes_rewrites_in_place_and_appends() {
        let text = "# header\nMOMO_PROFILE=normal   # 档位\nexport MOMO_FOO=old\n  MOMO_BAR=1\n";
        let mut changes = BTreeMap::new();
        changes.insert("MOMO_PROFILE".to_string(), "degraded".to_string());
        changes.insert("MOMO_FOO".to_string(), "new".to_string());
        changes.insert("MOMO_BAR".to_string(), "2".to_string());
        changes.insert("MOMO_NEW".to_string(), "v".to_string());
        let out = apply_changes(text, &changes);
        assert!(out.contains("# header\n"), "{out}");
        assert!(out.contains("MOMO_PROFILE=degraded   # 档位\n"), "{out}");
        assert!(out.contains("export MOMO_FOO=new\n"), "{out}");
        assert!(out.contains("  MOMO_BAR=2\n"), "{out}");
        assert!(out.ends_with("MOMO_NEW=v\n"), "{out}");
    }

    /// 值含空格/#/引号 → 双引号包裹且可再解析回同值（round-trip）。
    #[test]
    fn apply_changes_quotes_and_roundtrips() {
        let mut changes = BTreeMap::new();
        changes.insert("MOMO_A".to_string(), "hello world".to_string());
        changes.insert("MOMO_B".to_string(), "x#y".to_string());
        changes.insert("MOMO_C".to_string(), "q\"q".to_string());
        let out = apply_changes("", &changes);
        assert!(out.contains("MOMO_A=\"hello world\"\n"), "{out}");
        let dir = tmpdir("roundtrip");
        let f = dir.join(".env");
        fs::write(&f, &out).unwrap();
        let parsed = parse_env_file(&f).unwrap();
        assert_eq!(parsed["MOMO_A"], "hello world");
        assert_eq!(parsed["MOMO_B"], "x#y");
        assert_eq!(parsed["MOMO_C"], "q\"q");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 空值写 `KEY=`（string/path 留空口径）。
    #[test]
    fn apply_changes_empty_value() {
        let mut changes = BTreeMap::new();
        changes.insert("MOMO_A".to_string(), String::new());
        assert_eq!(apply_changes("", &changes), "MOMO_A=\n");
    }

    /// 原子写：目标出现新内容、无 tmp 残留；写→读回→.bak 链路。
    #[test]
    fn atomic_write_and_backup_roundtrip() {
        let dir = tmpdir("atomic");
        let f = dir.join(".env.normal");
        fs::write(&f, "MOMO_A=1\n").unwrap();

        let bak = backup_file(&f).unwrap();
        assert_eq!(bak, dir.join(".env.normal.bak"));
        assert_eq!(fs::read_to_string(&bak).unwrap(), "MOMO_A=1\n");

        atomic_write(&f, "MOMO_A=2\n").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "MOMO_A=2\n");
        // tmp 无残留
        assert!(!fs::read_dir(&dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp")));
        // .bak 仍是旧内容
        assert_eq!(fs::read_to_string(&bak).unwrap(), "MOMO_A=1\n");

        // 回滚：active 恢复、.bak 保留
        let from = restore_backup(&f).unwrap();
        assert_eq!(from, bak);
        assert_eq!(fs::read_to_string(&f).unwrap(), "MOMO_A=1\n");
        assert!(bak.is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 源缺失：不生成空备份；回滚无 bak → 明确错误。
    #[test]
    fn backup_and_restore_require_source() {
        let dir = tmpdir("nobak");
        let f = dir.join(".env.normal");
        assert!(backup_file(&f).is_err());
        assert!(restore_backup(&f).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// validate_value：enum/bool/int/url 域 + 空值纪律。
    #[test]
    fn validate_value_domains() {
        let spec = |kind: &str, values: &[&str]| KeySpec {
            name: "MOMO_X".into(),
            kind: kind.into(),
            values: values.iter().map(|s| s.to_string()).collect(),
            default: String::new(),
            sensitive: false,
            readers: vec![],
            files: vec![],
            examples: BTreeMap::new(),
            component: "common".into(),
            description: String::new(),
        };
        let e = spec("enum", &["a", "b"]);
        assert!(validate_value(&e, "a").is_ok());
        assert!(validate_value(&e, "z").is_err());
        assert!(validate_value(&e, "").is_err());
        assert!(validate_value(&spec("bool", &[]), "true").is_ok());
        assert!(validate_value(&spec("bool", &[]), "1").is_err());
        assert!(validate_value(&spec("int", &[]), "42").is_ok());
        assert!(validate_value(&spec("int", &[]), "x").is_err());
        assert!(validate_value(&spec("url", &[]), "ws://a:1/ws").is_ok());
        assert!(validate_value(&spec("url", &[]), "a:1").is_err());
        assert!(validate_value(&spec("string", &[]), "").is_ok());
        assert!(validate_value(&spec("path", &[]), "").is_ok());
    }

    /// mask：短值全遮，长值前 4 位 + …。
    #[test]
    fn mask_rules() {
        assert_eq!(mask(""), "(empty)");
        assert_eq!(mask("abc"), "…");
        assert_eq!(mask("sk-1234567"), "sk-1…");
    }

    /// diff：变更/新增/删除键都出现；敏感键打码。
    #[test]
    fn env_diff_masks_sensitive() {
        let old = map(&[
            ("MOMO_LLM_ENDPOINT", "http://a"),
            ("MOMO_LLM_API_KEY", "sk-supersecret"),
            ("MOMO_OLD", "1"),
        ]);
        let new = map(&[
            ("MOMO_LLM_ENDPOINT", "http://b"),
            ("MOMO_LLM_API_KEY", "sk-othersecret"),
            ("MOMO_NEW", "2"),
        ]);
        let mut sensitive = BTreeSet::new();
        sensitive.insert("MOMO_LLM_API_KEY".to_string());
        let diff = env_diff(&old, &new, &sensitive);
        let get = |k: &str| diff.iter().find(|d| d.key == k).unwrap();
        assert_eq!(get("MOMO_LLM_ENDPOINT").from.as_deref(), Some("http://a"));
        assert_eq!(get("MOMO_LLM_API_KEY").from.as_deref(), Some("sk-s…"));
        assert!(get("MOMO_OLD").to.is_none());
        assert!(get("MOMO_NEW").from.is_none());
        assert!(!diff.iter().any(|d| d.key == "MOMO_PROFILE"));
    }
}
