//! `config.schema.toml` 加载 + 漂移检测；口径见 config.md §6。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::output::AppError;

/// `<mod>/.env.example` 的模块 id（schema `files` 取值域）。
pub const FILES: &[&str] = &["app", "models", "infra"];
/// 消费模块 id（schema `readers` 取值域）。
pub const MODULES: &[&str] = &["app", "models", "infra"];
/// 配置页分组 id（schema `component` 取值域）：
/// 对齐 config.md §2.1 通用 / §2.2 编排器 / §2.3 模型服务 / §2.4 infra。
pub const COMPONENTS: &[&str] = &["common", "app", "models", "infra"];
const KEY_TYPES: &[&str] = &["string", "bool", "int", "path", "url", "enum"];
const SCHEMA_FILE: &str = "config.schema.toml";
const CONFIG_MD: &str = "docs/01-architecture/config.md";

#[derive(Debug, Deserialize)]
pub struct Schema {
    pub schema: u32,
    #[serde(default)]
    pub key: Vec<KeySpec>,
    #[serde(default)]
    pub passthrough: Vec<PassthroughSpec>,
}

#[derive(Debug, Deserialize)]
pub struct KeySpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub readers: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    /// 某 `.env.example` 示例值偏离 `default` 时的覆盖（key = FILES 元素）。
    #[serde(default)]
    pub examples: BTreeMap<String, String>,
    /// 配置页分 section（COMPONENTS 取值域；对齐 config.md §2 分组）。
    #[serde(default)]
    pub component: String,
    #[serde(default)]
    pub description: String,
}

/// 上游工具直通变量（非 `MOMO_` 前缀，config.md §2.5）。
#[derive(Debug, Deserialize)]
pub struct PassthroughSpec {
    pub name: String,
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub description: String,
}

fn default_kind() -> String {
    "string".into()
}

/// 一条漂移记录。`kind` ∈ missing / extra / value / duplicate / table / file / parse。
#[derive(Debug, Clone, Serialize)]
pub struct DriftItem {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub detail: String,
}

impl DriftItem {
    fn new(kind: &str, key: Option<&str>, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            key: key.map(|s| s.to_string()),
            detail: detail.into(),
        }
    }
}

/// 单个被检查来源（一个文件）的结果。
/// `skipped`：来源文件在 `root` 下不存在——拆仓后各仓只持有部分 file_id，
/// 缺失来源记 skip 不计漂移（config.md §6 拆仓口径）。
#[derive(Debug, Clone, Serialize)]
pub struct SourceReport {
    pub file: String,
    pub skipped: bool,
    pub drifts: Vec<DriftItem>,
}

impl SourceReport {
    pub fn ok(&self) -> bool {
        self.drifts.is_empty()
    }
}

impl Schema {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let text = fs::read_to_string(path)
            .map_err(|e| AppError::Config(format!("read {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, AppError> {
        let s: Schema = toml::from_str(text)
            .map_err(|e| AppError::Config(format!("parse {SCHEMA_FILE}: {e}")))?;
        s.validate()?;
        Ok(s)
    }

    /// schema 自身合法性：命名、枚举取值、readers/files 取值域、敏感项默认纪律等。
    /// 敏感项（config.md §6）：密钥型（string/url/path）`default` 必须为空；
    /// 开关型（enum/bool/int）`default` 是固定安全常量、豁免空默认；
    /// 两类 `examples` 都不得覆写真实值（只允许等于 `default`）。
    pub fn validate(&self) -> Result<(), AppError> {
        let mut errs: Vec<String> = vec![];
        if self.schema != 1 {
            errs.push(format!("schema version must be 1, got {}", self.schema));
        }
        let mut seen = BTreeSet::new();
        for k in &self.key {
            if !is_env_name(&k.name) || !k.name.starts_with("MOMO_") {
                errs.push(format!("key '{}' must be a MOMO_* env name", k.name));
            }
            if !seen.insert(k.name.clone()) {
                errs.push(format!("duplicate key '{}'", k.name));
            }
            if !KEY_TYPES.contains(&k.kind.as_str()) {
                errs.push(format!("key '{}': unknown type '{}'", k.name, k.kind));
            }
            match k.kind.as_str() {
                "enum" => {
                    if k.values.is_empty() {
                        errs.push(format!("key '{}': enum requires values", k.name));
                    } else if !k.values.iter().any(|v| v == &k.default) {
                        errs.push(format!(
                            "key '{}': default '{}' not in values",
                            k.name, k.default
                        ));
                    }
                }
                "bool" if k.default != "true" && k.default != "false" => {
                    errs.push(format!("key '{}': bool default must be true/false", k.name));
                }
                "int" if k.default.parse::<i64>().is_err() => {
                    errs.push(format!(
                        "key '{}': int default '{}' invalid",
                        k.name, k.default
                    ));
                }
                _ => {}
            }
            for r in &k.readers {
                if !MODULES.contains(&r.as_str()) {
                    errs.push(format!("key '{}': unknown reader '{r}'", k.name));
                }
            }
            if k.readers.is_empty() {
                errs.push(format!("key '{}': readers must not be empty", k.name));
            }
            if !COMPONENTS.contains(&k.component.as_str()) {
                errs.push(format!(
                    "key '{}': unknown component '{}' (expected common|app|models|infra)",
                    k.name, k.component
                ));
            }
            check_files(&k.name, &k.files, &mut errs, false);
            for f in k.examples.keys() {
                if !k.files.contains(f) {
                    errs.push(format!("key '{}': examples key '{f}' not in files", k.name));
                }
            }
            if k.sensitive {
                if !matches!(k.kind.as_str(), "enum" | "bool" | "int") && !k.default.is_empty() {
                    errs.push(format!(
                        "key '{}': sensitive key default must be empty",
                        k.name
                    ));
                }
                for (f, v) in &k.examples {
                    if v != &k.default {
                        errs.push(format!(
                            "key '{}': sensitive examples['{f}'] must equal default",
                            k.name
                        ));
                    }
                }
            }
            if k.description.trim().is_empty() {
                errs.push(format!("key '{}': description required", k.name));
            }
        }
        for p in &self.passthrough {
            if !is_env_name(&p.name) || p.name.starts_with("MOMO_") {
                errs.push(format!(
                    "passthrough '{}' must be a non-MOMO_ env name",
                    p.name
                ));
            }
            if !seen.insert(p.name.clone()) {
                errs.push(format!("duplicate key '{}'", p.name));
            }
            check_files(&p.name, &p.files, &mut errs, true);
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(AppError::Config(format!(
                "{SCHEMA_FILE} invalid:\n  {}",
                errs.join("\n  ")
            )))
        }
    }

    pub fn get(&self, name: &str) -> Option<&KeySpec> {
        self.key.iter().find(|k| k.name == name)
    }

    fn passthrough_get(&self, name: &str) -> Option<&PassthroughSpec> {
        self.passthrough.iter().find(|p| p.name == name)
    }

    /// `file_id`（app/models/infra）的 .env.example 期望键集合 → 期望值。
    /// 期望值 = `examples[file_id]` 覆盖，否则 `default`。
    fn expected_entries(&self, file_id: &str) -> BTreeMap<&str, &str> {
        let mut out = BTreeMap::new();
        for k in &self.key {
            if k.files.iter().any(|f| f == file_id) {
                out.insert(
                    k.name.as_str(),
                    k.examples.get(file_id).unwrap_or(&k.default).as_str(),
                );
            }
        }
        for p in &self.passthrough {
            if p.files.iter().any(|f| f == file_id) {
                out.insert(p.name.as_str(), p.default.as_str());
            }
        }
        out
    }
}

fn check_files(name: &str, files: &[String], errs: &mut Vec<String>, allow_empty: bool) {
    if files.is_empty() && !allow_empty {
        errs.push(format!("key '{name}': files must not be empty"));
    }
    for f in files {
        if !FILES.contains(&f.as_str()) {
            errs.push(format!("key '{name}': unknown files entry '{f}'"));
        }
    }
}

/// 从 `start` 向上找含 `config.schema.toml` 的目录（仓库根）。
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if dir.join(SCHEMA_FILE).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// 解析 dotenv 文本为 key→value（忽略注释行与空行，去重检测）。
fn parse_dotenv(text: &str, label: &str, drifts: &mut Vec<DriftItem>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (i, pair) in dotenvy::from_read_iter(text.as_bytes()).enumerate() {
        match pair {
            Ok((k, v)) => {
                if out.insert(k.clone(), v).is_some() {
                    drifts.push(DriftItem::new(
                        "duplicate",
                        Some(k.as_str()),
                        format!("{label}: key appears twice"),
                    ));
                }
            }
            Err(e) => drifts.push(DriftItem::new(
                "parse",
                None,
                format!("{label}: entry #{}: {e}", i + 1),
            )),
        }
    }
    out
}

/// 比对一份 dotenv 文本与 `file_id` 的期望集合（config.md §3 模板按 "app" 口径）。
pub fn check_env_content(
    text: &str,
    file_id: &str,
    label: &str,
    schema: &Schema,
) -> Vec<DriftItem> {
    let mut drifts = vec![];
    let actual = parse_dotenv(text, label, &mut drifts);
    let expected = schema.expected_entries(file_id);
    for (k, v) in &actual {
        match expected.get(k.as_str()) {
            None => drifts.push(DriftItem::new(
                "extra",
                Some(k),
                format!("{label}: key not in schema for '{file_id}'"),
            )),
            Some(want) if *want != v.as_str() => drifts.push(DriftItem::new(
                "value",
                Some(k),
                format!("{label}: '{v}' != expected '{want}'"),
            )),
            _ => {}
        }
    }
    for k in expected.keys() {
        if !actual.contains_key(*k) {
            drifts.push(DriftItem::new(
                "missing",
                Some(k),
                format!("{label}: schema key absent"),
            ));
        }
    }
    drifts
}

/// 读取并校验单个来源文件 `root/rel`：
/// 存在 → `check(text)` 漂移比对；不存在 → `skipped`（不计漂移）；
/// 存在但读取失败 → `file` drift。
fn check_source(
    root: &Path,
    rel: &str,
    check: impl FnOnce(&str) -> Vec<DriftItem>,
) -> SourceReport {
    match fs::read_to_string(root.join(rel)) {
        Ok(text) => SourceReport {
            file: rel.into(),
            skipped: false,
            drifts: check(&text),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SourceReport {
            file: rel.into(),
            skipped: true,
            drifts: vec![],
        },
        Err(e) => SourceReport {
            file: rel.into(),
            skipped: false,
            drifts: vec![DriftItem::new("file", None, format!("{rel}: {e}"))],
        },
    }
}

/// 全量检查：`root` 下 schema + 各 file_id 的 .env.example + config.md。
/// schema 缺失/损坏 → `AppError::Config`；`root` 下不存在的来源记 `skip`
/// （拆仓后各仓只校验自己持有的 file_id，config.md §6）。
pub fn check_repo(root: &Path) -> Result<Vec<SourceReport>, AppError> {
    let schema = Schema::load(&root.join(SCHEMA_FILE))?;
    let mut reports = vec![];
    for file_id in FILES {
        let rel = format!("{file_id}/.env.example");
        reports.push(check_source(root, &rel, |text| {
            check_env_content(text, file_id, &rel, &schema)
        }));
    }
    reports.push(check_source(root, CONFIG_MD, |text| {
        check_config_md(text, &schema)
    }));
    Ok(reports)
}

// ---------- config.md 解析 ----------

/// config.md 漂移检查：§2.1–2.4 键表（名称/默认值/敏感/读取组件）、
/// §2.5 直通表（名称）、§3 dotenv 模板（键集合与值，口径同 app 模板）。
pub fn check_config_md(text: &str, schema: &Schema) -> Vec<DriftItem> {
    let mut drifts = vec![];
    let mut section = ""; // 当前 `## N.` 一级小节
    let mut sub = ""; // 当前 `### 2.x` 小节
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();
    // 键 → 出现过的 §2.x 小节集合（component 对齐检查用；跨节引用键多处出现）。
    let mut seen_subs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut seen_pt: BTreeSet<String> = BTreeSet::new();
    let mut in_dotenv = false;
    let mut template = String::new();

    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("## ") {
            section = t.get(3..5).unwrap_or(""); // 如 "2."、"3."
            sub = "";
            in_dotenv = false;
            continue;
        }
        if t.starts_with("### ") {
            sub = t
                .get(4..)
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("");
            in_dotenv = false;
            continue;
        }
        if section == "3." && t.starts_with("```") {
            if in_dotenv {
                break; // 模板块结束：§3 只有一个 dotenv 块
            }
            in_dotenv = t.starts_with("```dotenv");
            continue;
        }
        if in_dotenv {
            template.push_str(line);
            template.push('\n');
            continue;
        }
        if section != "2." || !t.starts_with('|') {
            continue;
        }
        let cells = split_row(t);
        if cells.len() < 4 || !cells[0].contains('`') {
            continue; // 表头/分隔行
        }
        let names: Vec<String> = backticked(&cells[0])
            .into_iter()
            .filter(|n| is_env_name(n))
            .collect();
        if names.is_empty() {
            continue;
        }
        if sub == "2.5" {
            for name in names {
                if name.starts_with("MOMO_") {
                    drifts.push(DriftItem::new(
                        "table",
                        Some(name.as_str()),
                        "config.md §2.5: MOMO_ key belongs to §2.1–2.4",
                    ));
                } else if schema.passthrough_get(&name).is_some() {
                    seen_pt.insert(name);
                } else {
                    drifts.push(DriftItem::new(
                        "extra",
                        Some(name.as_str()),
                        "config.md §2.5: passthrough key not in schema",
                    ));
                }
            }
            continue;
        }
        let name = &names[0];
        let spec = match schema.get(name) {
            Some(s) => s,
            None => {
                drifts.push(DriftItem::new(
                    "extra",
                    Some(name.as_str()),
                    "config.md table: key not in schema",
                ));
                continue;
            }
        };
        seen_keys.insert(name.clone());
        seen_subs
            .entry(name.clone())
            .or_default()
            .insert(sub.to_string());
        // 默认值列（idx 2）：`x`→x、空→""
        let doc_default = doc_cell_default(&cells[2]);
        if doc_default != spec.default {
            drifts.push(DriftItem::new(
                "value",
                Some(name.as_str()),
                format!(
                    "config.md default '{doc_default}' != schema '{}'",
                    spec.default
                ),
            ));
        }
        // 敏感列（最后一列）：是/**是** → true
        let doc_sensitive = cells.last().is_some_and(|c| c.contains('是'));
        if doc_sensitive != spec.sensitive {
            drifts.push(DriftItem::new(
                "table",
                Some(name.as_str()),
                format!(
                    "config.md sensitive={doc_sensitive} != schema {}",
                    spec.sensitive
                ),
            ));
        }
        // §2.1 有「读取组件」列（idx 4）；§2.2–2.4 按小节隐含消费模块
        if sub == "2.1" && cells.len() >= 6 {
            let doc_readers = parse_readers_cell(&cells[4]);
            let want: BTreeSet<&str> = spec.readers.iter().map(String::as_str).collect();
            if doc_readers != want {
                drifts.push(DriftItem::new(
                    "table",
                    Some(name.as_str()),
                    format!("config.md readers {doc_readers:?} != schema {want:?}"),
                ));
            }
        }
        let primary = match sub {
            "2.2" => Some("app"),
            "2.3" => Some("models"),
            "2.4" => Some("infra"),
            _ => None,
        };
        if let Some(m) = primary {
            if !spec.readers.iter().any(|r| r == m) {
                drifts.push(DriftItem::new(
                    "table",
                    Some(name.as_str()),
                    format!("config.md §{sub} key: schema readers lack '{m}'"),
                ));
            }
        }
    }

    for k in &schema.key {
        if !seen_keys.contains(k.name.as_str()) {
            drifts.push(DriftItem::new(
                "missing",
                Some(k.name.as_str()),
                "config.md §2 table: schema key absent",
            ));
            continue;
        }
        // component ↔ §2 小节对齐：键须在与其 component 对应的小节出现过
        // （§2.1=common §2.2=app §2.3=models §2.4=infra；跨节引用任一命中即可）。
        let subs = seen_subs.get(&k.name).cloned().unwrap_or_default();
        if !subs
            .iter()
            .any(|s| sub_component(s) == Some(k.component.as_str()))
        {
            drifts.push(DriftItem::new(
                "table",
                Some(k.name.as_str()),
                format!(
                    "config.md §2 sections {subs:?} != schema component '{}'",
                    k.component
                ),
            ));
        }
    }
    for p in &schema.passthrough {
        if !seen_pt.contains(p.name.as_str()) {
            drifts.push(DriftItem::new(
                "missing",
                Some(p.name.as_str()),
                "config.md §2.5: passthrough key absent",
            ));
        }
    }

    // §3 dotenv 模板：期望口径与 app/.env.example 相同（完整模板）。
    if template.is_empty() {
        drifts.push(DriftItem::new(
            "table",
            None,
            "config.md §3: ```dotenv template block not found",
        ));
    } else {
        drifts.extend(check_env_content(
            &template,
            "app",
            "config.md §3 template",
            schema,
        ));
    }
    drifts
}

/// markdown 表格行 → 去 `|`/trim 后的 cell 列表。
fn split_row(line: &str) -> Vec<String> {
    line.trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

/// config.md §2.x 小节编号 → schema `component` 取值。
fn sub_component(sub: &str) -> Option<&'static str> {
    match sub {
        "2.1" => Some("common"),
        "2.2" => Some("app"),
        "2.3" => Some("models"),
        "2.4" => Some("infra"),
        _ => None,
    }
}

/// cell 内全部 `` `...` `` 段内容。
fn backticked(cell: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = cell;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

/// 默认值 cell → schema 形式：取首个反引号内容或裸文本；「空」→ ""。
fn doc_cell_default(cell: &str) -> String {
    let v = backticked(cell)
        .into_iter()
        .next()
        .unwrap_or_else(|| cell.trim().to_string());
    if v == "空" {
        String::new()
    } else {
        v
    }
}

/// 「读取组件」cell（§2.1）：取 `；`/`（` 前段，「全部」→ 三模块，否则按子串匹配。
fn parse_readers_cell(cell: &str) -> BTreeSet<&'static str> {
    let seg = cell
        .split('；')
        .next()
        .unwrap_or(cell)
        .split('（')
        .next()
        .unwrap_or(cell);
    if seg.contains("全部") {
        return MODULES.iter().copied().collect();
    }
    MODULES
        .iter()
        .copied()
        .filter(|m| seg.contains(m))
        .collect()
}

/// 合法 env 名：大写字母开头，仅大写/数字/下划线。
fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI_SCHEMA: &str = r#"
schema = 1
[[key]]
name = "MOMO_FOO"
type = "string"
default = "x"
sensitive = false
readers = ["app"]
files = ["app"]
component = "common"
description = "d"
[[passthrough]]
name = "HF_ENDPOINT"
type = "url"
default = "https://hf-mirror.com"
files = ["app"]
description = "mirror"
"#;

    #[test]
    fn schema_parse_and_validate() {
        let s = Schema::parse(MINI_SCHEMA).unwrap();
        assert_eq!(s.key.len(), 1);
        assert_eq!(s.expected_entries("app").len(), 2);
        assert!(s.expected_entries("models").is_empty());
    }

    #[test]
    fn env_content_missing_extra_value() {
        let s = Schema::parse(MINI_SCHEMA).unwrap();
        let d = check_env_content("MOMO_FOO=y\nEXTRA=1\n", "app", "t/.env.example", &s);
        let kinds: Vec<&str> = d.iter().map(|x| x.kind.as_str()).collect();
        assert!(kinds.contains(&"value"));
        assert!(kinds.contains(&"extra"));
        // HF_ENDPOINT 缺失 → missing
        assert!(d.iter().any(|x| x.key.as_deref() == Some("HF_ENDPOINT")));
    }

    #[test]
    fn env_content_clean() {
        let s = Schema::parse(MINI_SCHEMA).unwrap();
        let d = check_env_content(
            "MOMO_FOO=x # comment\nHF_ENDPOINT=https://hf-mirror.com\n# UV_INDEX_URL=x\n",
            "app",
            "t",
            &s,
        );
        assert!(d.is_empty(), "{d:?}");
    }

    /// 密钥型敏感项 default 必须为空（密钥默认值不入库）。
    #[test]
    fn sensitive_secret_requires_empty_default() {
        let text = MINI_SCHEMA.replace(
            "default = \"x\"\nsensitive = false",
            "default = \"x\"\nsensitive = true",
        );
        let err = Schema::parse(&text).unwrap_err().to_string();
        assert!(err.contains("sensitive key default must be empty"), "{err}");
        let ok = text.replace("default = \"x\"", "default = \"\"");
        Schema::parse(&ok).unwrap();
    }

    /// 开关型敏感项（enum/bool/int）保留安全常量默认，豁免空默认规则。
    #[test]
    fn sensitive_switch_keeps_constant_default() {
        let text = MINI_SCHEMA.replace(
            r#"name = "MOMO_FOO"
type = "string"
default = "x"
sensitive = false"#,
            r#"name = "MOMO_FOO"
type = "enum"
values = ["on", "off"]
default = "off"
sensitive = true"#,
        );
        Schema::parse(&text).unwrap();
    }

    /// component 与 config.md §2 小节对齐：键出现在与其 component 不符的小节 → drift。
    #[test]
    fn component_matches_doc_subsection() {
        let s = Schema::parse(MINI_SCHEMA).unwrap(); // component = "common"
                                                     // 写在 §2.3（models）但 component=common → drift；且无 missing。
        let md = "# c\n\n## 2. 变量全集\n\n### 2.3 模型服务\n\n\
                  | 名称 | 含义 | 默认值 | 取值 | 敏感 |\n|---|---|---|---|---|\n\
                  | `MOMO_FOO` | d | `x` | str | 否 |\n\n### 2.5 直通\n\n\
                  | 名称 | 含义 | 默认值 | 说明 |\n|---|---|---|---|\n\
                  | `HF_ENDPOINT` | m | `https://hf-mirror.com` | m |\n\n\
                  ## 3. `.env.example`\n\n```dotenv\nMOMO_FOO=x\nHF_ENDPOINT=https://hf-mirror.com\n```\n";
        let d = check_config_md(md, &s);
        assert!(
            d.iter()
                .any(|x| x.key.as_deref() == Some("MOMO_FOO") && x.detail.contains("component")),
            "{d:?}"
        );
        // component=common 落在 §2.1 → 无该项 drift。
        let md2 = md.replace("### 2.3 模型服务", "### 2.1 通用");
        let d2 = check_config_md(&md2, &s);
        assert!(
            !d2.iter().any(|x| x.key.as_deref() == Some("MOMO_FOO")),
            "{d2:?}"
        );
    }

    /// 敏感项 examples 不得覆写真实值（只允许等于 default）。
    #[test]
    fn sensitive_examples_must_equal_default() {
        let text = MINI_SCHEMA.replace(
            "sensitive = false",
            "sensitive = true\nexamples = { app = \"real-value\" }",
        );
        let err = Schema::parse(&text).unwrap_err().to_string();
        assert!(
            err.contains("sensitive examples['app'] must equal default"),
            "{err}"
        );
    }
}
