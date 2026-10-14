//! `momoctl gateway` 的 TOML 配置模型。
//! 口径见 docs/01-architecture/gateway.md §2：
//! 顶层策略字段 + `[[llm.backends]]`（分权 LB）+ `[[asr.upstream]]`/`[[tts.upstream]]`
//! （透明反代）。`endpoint`/`api_key` 内 `${VAR}` 加载期展开为环境变量。

use std::path::Path;

use serde::Deserialize;

use crate::output::AppError;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7830";
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_MAX_ATTEMPTS: usize = 3;
const DEFAULT_COOLDOWN_THRESHOLD: u32 = 3;
const DEFAULT_COOLDOWN_MS: u64 = 30_000;
const DEFAULT_HEALTH_INTERVAL_MS: u64 = 10_000;
const DEFAULT_HEALTH_PATH: &str = "/healthz";

fn default_request_timeout_ms() -> u64 {
    DEFAULT_REQUEST_TIMEOUT_MS
}
fn default_max_attempts() -> usize {
    DEFAULT_MAX_ATTEMPTS
}
fn default_cooldown_threshold() -> u32 {
    DEFAULT_COOLDOWN_THRESHOLD
}
fn default_cooldown_ms() -> u64 {
    DEFAULT_COOLDOWN_MS
}
fn default_health_interval_ms() -> u64 {
    DEFAULT_HEALTH_INTERVAL_MS
}
fn default_asr_paths() -> Vec<String> {
    vec!["/v1/listen".to_string()]
}
fn default_tts_paths() -> Vec<String> {
    vec!["/v1/audio/speech".to_string()]
}

/// gateway 顶层配置（对应一个 TOML 文件）。
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// 监听地址；`--listen` / `MOMO_GATEWAY_LISTEN` 优先于此。
    pub listen: Option<String>,
    /// 单次上游请求超时（ms）。
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// 单请求最大尝试次数（含首次）。
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
    /// 连续失败达到该次数进入熔断。
    #[serde(default = "default_cooldown_threshold")]
    pub cooldown_threshold: u32,
    /// 熔断冷却时长（ms）。
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
    /// 后台健康探测周期（ms）；0 = 关闭。
    #[serde(default = "default_health_interval_ms")]
    pub health_interval_ms: u64,
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub asr: ProxySection,
    #[serde(default)]
    pub tts: ProxySection,
}

/// `[[llm.backends]]` 所属 section。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LlmSection {
    #[serde(default)]
    pub backends: Vec<BackendSpec>,
}

/// `[[asr.upstream]]` / `[[tts.upstream]]` 所属 section：
/// `paths` 是 gateway 侧对外暴露路径，`upstream` 是透明反代目标。
/// 默认空；对外路径缺省值由 `asr_paths()`/`tts_paths()` 按 upstream 非空时补。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProxySection {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub upstream: Vec<BackendSpec>,
}

/// 单个 upstream 定义（llm backend 与 asr/tts upstream 同构）。
#[derive(Debug, Clone, Deserialize)]
pub struct BackendSpec {
    /// 展示/指标名；缺省取 endpoint。
    pub name: Option<String>,
    /// base URL（http/https/ws/wss），转发时拼接客户端原始 path+query。
    pub endpoint: String,
    /// 优先级，越小越优先（仅 llm 语义化；asr/tts 视为单组）。
    pub priority: Option<i64>,
    /// 组内加权轮询权重，缺省 1。
    pub weight: Option<u32>,
    /// 后台探测路径，缺省 `/healthz`。
    pub health_path: Option<String>,
    /// 非空则覆盖客户端 `Authorization`（`Bearer` 缺省自动补前缀）。
    pub api_key: Option<String>,
    /// 非空则把请求 path 改写为该值（query 透传）；asr/tts 上游改路径用。
    pub path: Option<String>,
}

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AppError::Config(format!("read {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| AppError::Config(format!("{}: {e}", path.display())))
    }

    pub fn parse(text: &str) -> Result<Self, AppError> {
        let mut cfg: GatewayConfig = toml::from_str(text)
            .map_err(|e| AppError::Config(format!("parse gateway config: {e}")))?;
        cfg.expand_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// `endpoint`/`api_key` 内 `${VAR}` 展开为进程环境变量（密钥不明文入库）。
    fn expand_env(&mut self) {
        for spec in self
            .llm
            .backends
            .iter_mut()
            .chain(self.asr.upstream.iter_mut())
            .chain(self.tts.upstream.iter_mut())
        {
            spec.endpoint = expand_env(&spec.endpoint);
            if let Some(k) = &mut spec.api_key {
                *k = expand_env(k);
            }
        }
    }

    /// 结构校验：endpoint scheme、path 形态、asr/tts 路径冲突、至少一个 upstream。
    fn validate(&self) -> Result<(), AppError> {
        let mut errs: Vec<String> = vec![];
        let mut total = 0usize;
        for (pool, specs) in [
            ("llm.backends", &self.llm.backends),
            ("asr.upstream", &self.asr.upstream),
            ("tts.upstream", &self.tts.upstream),
        ] {
            total += specs.len();
            for (i, s) in specs.iter().enumerate() {
                let label = format!("{pool}[{i}]");
                if s.endpoint.trim().is_empty() {
                    errs.push(format!("{label}: endpoint required"));
                    continue;
                }
                if !["http://", "https://", "ws://", "wss://"]
                    .iter()
                    .any(|p| s.endpoint.starts_with(p))
                {
                    errs.push(format!(
                        "{label}: endpoint '{}' must start with http(s):// or ws(s)://",
                        s.endpoint
                    ));
                }
                if let Some(p) = &s.path {
                    if !p.starts_with('/') {
                        errs.push(format!("{label}: path '{p}' must start with '/'"));
                    }
                }
                if matches!(s.weight, Some(0)) {
                    errs.push(format!("{label}: weight must be >= 1"));
                }
            }
        }
        for (pool, paths) in [("asr", &self.asr.paths), ("tts", &self.tts.paths)] {
            for p in paths {
                if !p.starts_with('/') {
                    errs.push(format!("{pool}.paths: '{p}' must start with '/'"));
                }
            }
        }
        // asr/tts 对外路径不得重叠（同一路径只能反代到一个池）。
        let overlap: Vec<&String> = self
            .asr
            .paths
            .iter()
            .filter(|p| self.tts.paths.contains(*p))
            .collect();
        if !overlap.is_empty() {
            errs.push(format!(
                "asr.paths and tts.paths overlap: {}",
                overlap
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if self.max_attempts == 0 {
            errs.push("max_attempts must be >= 1".into());
        }
        if self.cooldown_threshold == 0 {
            errs.push("cooldown_threshold must be >= 1".into());
        }
        if total == 0 {
            errs.push(
                "no upstreams configured (llm.backends / asr.upstream / tts.upstream)".into(),
            );
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(AppError::Config(format!(
                "gateway config invalid:\n  {}",
                errs.join("\n  ")
            )))
        }
    }

    /// asr 对外路径（配置缺省时取默认 `/v1/listen`）。
    pub fn asr_paths(&self) -> Vec<String> {
        if self.asr.paths.is_empty() && !self.asr.upstream.is_empty() {
            default_asr_paths()
        } else {
            self.asr.paths.clone()
        }
    }

    /// tts 对外路径（配置缺省时取默认 `/v1/audio/speech`）。
    pub fn tts_paths(&self) -> Vec<String> {
        if self.tts.paths.is_empty() && !self.tts.upstream.is_empty() {
            default_tts_paths()
        } else {
            self.tts.paths.clone()
        }
    }
}

impl BackendSpec {
    /// 归一化为运行时 backend 字段（name/weight/priority/health_path 补默认）。
    pub fn normalized(
        &self,
    ) -> (
        String,
        String,
        i64,
        u32,
        String,
        Option<String>,
        Option<String>,
    ) {
        let endpoint = self.endpoint.trim_end_matches('/').to_string();
        let name = self.name.clone().unwrap_or_else(|| endpoint.clone());
        (
            name,
            endpoint,
            self.priority.unwrap_or(0),
            self.weight.unwrap_or(1),
            self.health_path
                .clone()
                .unwrap_or_else(|| DEFAULT_HEALTH_PATH.to_string()),
            self.api_key.clone().filter(|k| !k.is_empty()),
            self.path.clone(),
        )
    }
}

/// `${VAR}` 展开：未设置的变量展开为空串；非 `${...}` 片段原样保留。
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                out.push_str(&std::env::var(name).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                // 未闭合的 `${` 原样保留，不吞字符。
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_llm_config() {
        let cfg = GatewayConfig::parse(
            r#"
[[llm.backends]]
endpoint = "http://a:1"
"#,
        )
        .unwrap();
        assert_eq!(cfg.request_timeout_ms, DEFAULT_REQUEST_TIMEOUT_MS);
        assert_eq!(cfg.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(cfg.llm.backends.len(), 1);
        let (_, ep, prio, w, hp, _, _) = cfg.llm.backends[0].normalized();
        assert_eq!(ep, "http://a:1");
        assert_eq!(prio, 0);
        assert_eq!(w, 1);
        assert_eq!(hp, "/healthz");
    }

    #[test]
    fn defaults_paths_apply_when_upstreams_present() {
        let cfg = GatewayConfig::parse(
            r#"
[[asr.upstream]]
endpoint = "ws://127.0.0.1:7810"
[[tts.upstream]]
endpoint = "http://127.0.0.1:7811"
"#,
        )
        .unwrap();
        assert_eq!(cfg.asr_paths(), vec!["/v1/listen".to_string()]);
        assert_eq!(cfg.tts_paths(), vec!["/v1/audio/speech".to_string()]);
    }

    #[test]
    fn empty_config_rejected() {
        let e = GatewayConfig::parse("listen = \"127.0.0.1:1\"\n").unwrap_err();
        assert!(e.to_string().contains("no upstreams"), "{e}");
    }

    #[test]
    fn bad_scheme_rejected() {
        let e = GatewayConfig::parse("[[llm.backends]]\nendpoint = \"ftp://x\"\n").unwrap_err();
        assert!(e.to_string().contains("http"), "{e}");
    }

    #[test]
    fn expand_env_replaces_and_keeps_rest() {
        std::env::set_var("MOMO_GW_TEST_KEY", "sekrit");
        assert_eq!(
            expand_env("https://x/${MOMO_GW_TEST_KEY}/y"),
            "https://x/sekrit/y"
        );
        assert_eq!(expand_env("a${MOMO_GW_TEST_MISSING}b"), "ab");
        assert_eq!(expand_env("keep ${unclosed"), "keep ${unclosed");
    }
}
