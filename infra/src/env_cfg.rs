//! dotenvy 加载 + `MOMO_*` 读取与 profile 定位。
//! env 全集权威：momo-docs `01-architecture/config.md`；本模块只读不定义新键。

use std::path::{Path, PathBuf};

use crate::cli::Profile;
use crate::output::AppError;

/// 加载 `.env`：显式 `--env-file` 不存在时报错（exit 2）；默认自动发现失败时静默。
pub fn load_dotenv(path: Option<&Path>) -> Result<(), AppError> {
    match path {
        Some(p) => {
            dotenvy::from_filename(p).map_err(|e| {
                AppError::Config(format!("cannot load env file {}: {e}", p.display()))
            })?;
        }
        None => {
            let _ = dotenvy::dotenv();
        }
    }
    Ok(())
}

/// `MOMO_*` 读取门面（dotenvy 已注入进程 env）。
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvCfg;

/// 无默认值的必需键：缺失在 doctor 中记 fail。
/// （其余键均有 `config.md` 内置默认，缺失仅提示。）
pub const REQUIRED_KEYS: &[&str] = &[
    "MOMO_ORCH_LISTEN",
    "MOMO_LLM_ENDPOINT",
    "MOMO_LLM_MODEL",
    "MOMO_ASR_ENDPOINT",
    "MOMO_TTS_ENDPOINT",
];

/// 敏感键：doctor 只查「是否设置」，不回显值（config.md §4）。
pub const SENSITIVE_KEYS: &[&str] = &["MOMO_LLM_API_KEY"];

/// 有内置默认、但值得在 doctor 中报告取值的键。
pub const DEFAULTED_KEYS: &[&str] = &[
    "MOMO_MODELS_DIR",
    "MOMO_ASR_LISTEN",
    "MOMO_TTS_LISTEN",
    "MOMO_BRAIN_BACKEND",
    "MOMO_BRAIN_PORT",
    "MOMO_BRAIN_MODEL_PATH",
];

impl EnvCfg {
    pub fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }

    /// 档位解析：CLI flag > `MOMO_PROFILE` > 默认 normal。
    pub fn profile(&self, cli: Option<Profile>) -> Profile {
        if let Some(p) = cli {
            return p;
        }
        match self.get("MOMO_PROFILE").as_deref() {
            Some("degraded") => Profile::Degraded,
            _ => Profile::Normal,
        }
    }

    /// `MOMO_MODELS_DIR`，默认 `~/.cache/momo/models`，展开 `~`。
    pub fn models_dir(&self) -> PathBuf {
        match self.get("MOMO_MODELS_DIR") {
            Some(v) => expand_tilde(&v),
            None => default_models_dir(),
        }
    }

    /// `MOMO_ORCH_LISTEN`，默认 `127.0.0.1:7800`。
    pub fn orch_listen(&self) -> String {
        self.get("MOMO_ORCH_LISTEN")
            .unwrap_or_else(|| "127.0.0.1:7800".into())
    }

    /// `MOMO_ASR_LISTEN`，默认 `127.0.0.1:7810`。
    pub fn asr_listen(&self) -> String {
        self.get("MOMO_ASR_LISTEN")
            .unwrap_or_else(|| "127.0.0.1:7810".into())
    }

    /// `MOMO_TTS_LISTEN`，默认 `127.0.0.1:7811`。
    pub fn tts_listen(&self) -> String {
        self.get("MOMO_TTS_LISTEN")
            .unwrap_or_else(|| "127.0.0.1:7811".into())
    }

    /// `MOMO_LLM_ENDPOINT`（config.md 默认 `http://mac-studio.local:7820/v1`）。
    /// 返回 None 表示未配置，doctor 端点组按 fail 处理。
    pub fn llm_endpoint(&self) -> Option<String> {
        self.get("MOMO_LLM_ENDPOINT")
    }

    /// `MOMO_WEB_TOKEN`（无默认）：`momoctl web` 鉴权 token；
    /// `--listen` 绑出 loopback 时必填。返回 None 表示未配置。
    pub fn web_token(&self) -> Option<String> {
        self.get("MOMO_WEB_TOKEN")
    }

    /// `MOMO_BRAIN_PORT`，默认 7820。
    pub fn brain_port(&self) -> u16 {
        self.get("MOMO_BRAIN_PORT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(7820)
    }
}

/// `~` / `~/...` 展开为 `$HOME`；其余原样返回。
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

/// `MOMO_MODELS_DIR` 内置默认：`~/.cache/momo/models`。
/// 逐段 join，不把 `/` 分隔写死在字符串里（Windows 用 `\`）。
fn default_models_dir() -> PathBuf {
    home_dir().join(".cache").join("momo").join("models")
}

/// home 解析的候选环境变量键（按优先级）：
/// Windows 无 `HOME`，主键是 `USERPROFILE`（MSYS/Git-Bash 会额外设 `HOME`）；unix 用 `HOME`。
#[cfg(windows)]
const HOME_ENV_KEYS: &[&str] = &["USERPROFILE", "HOME"];
#[cfg(not(windows))]
const HOME_ENV_KEYS: &[&str] = &["HOME"];

/// 候选键全空时的兜底根：unix `/`；Windows `.`（相对当前工作目录，避免落到文件系统根 `/`）。
#[cfg(windows)]
const HOME_FALLBACK: &str = ".";
#[cfg(not(windows))]
const HOME_FALLBACK: &str = "/";

/// home 目录：按平台候选键取第一个非空值，全空用平台兜底根。
fn home_dir() -> PathBuf {
    resolve_home(HOME_ENV_KEYS, Path::new(HOME_FALLBACK), |k| {
        std::env::var(k).ok()
    })
}

/// `resolve_home` 的取值逻辑与平台无关，抽成纯函数便于单测：
/// 依次取 `keys` 中第一个非空值转 `PathBuf`；全空则返回 `fallback`。
fn resolve_home(keys: &[&str], fallback: &Path, get: impl Fn(&str) -> Option<String>) -> PathBuf {
    keys.iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 候选键取值：忽略空串，命中即返回。
    #[test]
    fn resolve_home_takes_first_nonempty_key() {
        let get = |k: &str| match k {
            "USERPROFILE" => Some(String::new()),
            "HOME" => Some("C:\\Users\\momo".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_home(&["USERPROFILE", "HOME"], Path::new("."), get),
            PathBuf::from("C:\\Users\\momo")
        );
    }

    /// Windows 键序下 `USERPROFILE` 优先于 `HOME`。
    #[test]
    fn resolve_home_prefers_userprofile_on_windows_keys() {
        let get = |k: &str| match k {
            "USERPROFILE" => Some("C:\\Users\\momo".to_string()),
            "HOME" => Some("/msys/home".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_home(&["USERPROFILE", "HOME"], Path::new("."), get),
            PathBuf::from("C:\\Users\\momo")
        );
    }

    /// 全空时用兜底根：Windows 不落到 `/`。
    #[test]
    fn resolve_home_falls_back_without_unix_root_on_windows() {
        let none = |_: &str| None;
        assert_eq!(
            resolve_home(&["USERPROFILE", "HOME"], Path::new("."), none),
            PathBuf::from(".")
        );
    }

    /// 当前平台的键表与兜底根。
    #[cfg(unix)]
    #[test]
    fn home_keys_are_home_on_unix() {
        assert_eq!(HOME_ENV_KEYS, &["HOME"]);
        assert_eq!(HOME_FALLBACK, "/");
    }

    #[cfg(windows)]
    #[test]
    fn home_keys_are_userprofile_on_windows() {
        assert_eq!(HOME_ENV_KEYS, &["USERPROFILE", "HOME"]);
        assert_eq!(HOME_FALLBACK, ".");
    }

    /// `~` / `~/...` 展开到 home，其余原样。
    #[test]
    fn expand_tilde_only_rewrites_leading_tilde() {
        assert_eq!(expand_tilde("~"), home_dir());
        assert_eq!(expand_tilde("~/models"), home_dir().join("models"));
        assert_eq!(expand_tilde("/abs/models"), PathBuf::from("/abs/models"));
        assert_eq!(expand_tilde("rel/models"), PathBuf::from("rel/models"));
    }

    /// 内置默认值 `~/.cache/momo/models` 落在 home 下，且不含写死的 unix 根。
    #[test]
    fn default_models_dir_is_under_home() {
        let dir = default_models_dir();
        assert_eq!(dir, home_dir().join(".cache").join("momo").join("models"));
        assert!(dir.ends_with(".cache/momo/models"), "{}", dir.display());
    }

    /// macOS/Linux 默认值不回归：仍是 `$HOME/.cache/momo/models`。
    #[cfg(unix)]
    #[test]
    fn default_models_dir_unchanged_on_unix() {
        let home = std::env::var("HOME").expect("HOME must be set on unix");
        assert_eq!(
            default_models_dir(),
            PathBuf::from(home).join(".cache/momo/models")
        );
    }
}
