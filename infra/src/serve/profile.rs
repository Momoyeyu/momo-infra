//! `profiles/*.toml` → 组件清单与启动顺序。
//! profile 是组件表不是用户配置（momo-infra.md）；normal/degraded 差异即部署档位差异。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::output::AppError;

#[derive(Debug, Clone, Deserialize)]
pub struct ServeProfile {
    pub profile: ProfileMeta,
    #[serde(rename = "component", default)]
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Component {
    pub name: String,
    /// 启动顺序，升序拉起；down 逆序。
    #[serde(default)]
    pub order: u32,
    /// 外部端点：只探活不拉起（如 normal 档的 mac-studio MLX 大脑）。
    #[serde(default)]
    pub external: bool,
    /// 仅当 MOMO_LLM_ENDPOINT 指向 loopback 时才 spawn
    /// （degraded 本地 llama-server；端点指 DeepSeek 时跳过）。
    #[serde(default)]
    pub local_endpoint_only: bool,
    /// 工作目录（如 python 服务在 momo-models 仓内 `uv run`）。
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// 本地组件就绪 URL（轮询至 200 或超时）。
    #[serde(default)]
    pub ready: Option<String>,
    /// external 组件：端点取自该 env 键（如 MOMO_LLM_ENDPOINT）。
    #[serde(default)]
    pub probe_env: Option<String>,
    /// external 组件：端点附加路径（如 /models）。
    #[serde(default)]
    pub probe_path: Option<String>,
    /// 组件级 env 覆盖（叠加在 .env 之上）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl ServeProfile {
    pub fn load(path: &Path) -> Result<Self, AppError> {
        let text = fs::read_to_string(path)
            .map_err(|e| AppError::Config(format!("read profile {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    /// 内置 profile（profiles/*.toml 编译期内嵌，任意 cwd 可用）。
    pub fn load_builtin(name: &str) -> Result<Self, AppError> {
        let text = match name {
            "normal" => include_str!("../../profiles/normal.toml"),
            "degraded" => include_str!("../../profiles/degraded.toml"),
            other => {
                return Err(AppError::Config(format!(
                    "unknown profile '{other}' (expected normal|degraded)"
                )))
            }
        };
        Self::parse(text)
    }

    pub fn parse(text: &str) -> Result<Self, AppError> {
        let p: ServeProfile = toml::from_str(text)
            .map_err(|e| AppError::Config(format!("parse profile toml: {e}")))?;
        p.validate()?;
        Ok(p)
    }

    fn validate(&self) -> Result<(), AppError> {
        for c in &self.components {
            if c.name.trim().is_empty() {
                return Err(AppError::Config("component with empty name".into()));
            }
            if !c.external && c.cmd.is_none() {
                return Err(AppError::Config(format!(
                    "component '{}': non-external component needs cmd",
                    c.name
                )));
            }
            if c.external && c.probe_env.is_none() && c.ready.is_none() {
                return Err(AppError::Config(format!(
                    "component '{}': external component needs probe_env or ready",
                    c.name
                )));
            }
        }
        Ok(())
    }

    /// 按 order 升序的启动序列（external 组件也按序插入，up 时探活）。
    pub fn start_order(&self) -> Vec<&Component> {
        let mut v: Vec<&Component> = self.components.iter().collect();
        v.sort_by_key(|c| c.order);
        v
    }

    /// down 顺序：逆序。
    pub fn stop_order(&self) -> Vec<&Component> {
        let mut v = self.start_order();
        v.reverse();
        v
    }

    /// 该组件在当前 env 下是否应 spawn。
    /// `local_endpoint_only`：MOMO_LLM_ENDPOINT 非 loopback（如 DeepSeek API）时跳过。
    pub fn should_spawn(&self, c: &Component, llm_endpoint: Option<&str>) -> bool {
        if c.external {
            return false;
        }
        if c.local_endpoint_only {
            return llm_endpoint.is_some_and(is_loopback_endpoint);
        }
        true
    }

    /// 参数模板展开：`${VAR}` 取进程 env。
    pub fn expand_args(c: &Component) -> Vec<String> {
        c.args.iter().map(|a| expand_var(a)).collect()
    }
}

fn is_loopback_endpoint(ep: &str) -> bool {
    ep.contains("127.0.0.1") || ep.contains("localhost") || ep.contains("[::1]")
}

/// `${VAR}` 占位展开；未设置的变量保留原样。
fn expand_var(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        match tail.find('}') {
            Some(j) => {
                let key = &tail[..j];
                match std::env::var(key) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push_str("${");
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &tail[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
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
    fn builtin_profiles_parse() {
        for name in ["normal", "degraded"] {
            let p = ServeProfile::load_builtin(name).unwrap();
            assert_eq!(p.profile.name, name);
            assert!(!p.components.is_empty());
            // order 递增排列后首元素应为最小 order
            let ordered = p.start_order();
            let first = ordered.first().unwrap();
            assert!(p.components.iter().all(|c| c.order >= first.order));
        }
    }

    #[test]
    fn arg_expansion() {
        std::env::set_var("MOMO_TEST_EXPAND", "ok");
        let c = Component {
            name: "t".into(),
            order: 0,
            external: false,
            local_endpoint_only: false,
            cwd: None,
            cmd: Some("echo".into()),
            args: vec!["${MOMO_TEST_EXPAND}-x".into(), "${UNSET_VAR_42}".into()],
            ready: None,
            probe_env: None,
            probe_path: None,
            env: BTreeMap::new(),
        };
        let args = ServeProfile::expand_args(&c);
        assert_eq!(args[0], "ok-x");
        assert_eq!(args[1], "${UNSET_VAR_42}");
        std::env::remove_var("MOMO_TEST_EXPAND");
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_endpoint("http://127.0.0.1:7820/v1"));
        assert!(!is_loopback_endpoint("https://api.deepseek.com/v1"));
    }
}
