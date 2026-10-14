//! HTTP 探活：reqwest blocking + 超时。doctor / serve 共用。

use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::Method;

/// 探活超时：doc 口径 3s。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Probe {
    /// 收到任何 HTTP 响应即为 true（含 4xx/5xx；DNS/连接失败为 false）。
    pub reachable: bool,
    pub status: Option<u16>,
    pub latency_ms: u128,
    pub error: Option<String>,
}

impl Probe {
    /// 2xx 才算端点就绪（/healthz、/v1/models 等）。
    pub fn ready(&self) -> bool {
        self.status.is_some_and(|s| (200..300).contains(&s))
    }

    pub fn describe(&self) -> String {
        match (self.status, &self.error) {
            (Some(s), _) => format!("HTTP {s} in {}ms", self.latency_ms),
            (None, Some(e)) => e.clone(),
            (None, None) => "unreachable".into(),
        }
    }
}

/// HEAD 探活（huggingface.co / hf-mirror.com 等可达性检查）。
pub fn probe_head(url: &str) -> Probe {
    probe(url, Method::HEAD, PROBE_TIMEOUT)
}

/// GET 探活（/healthz、/v1/models）。
pub fn probe_get(url: &str) -> Probe {
    probe(url, Method::GET, PROBE_TIMEOUT)
}

fn probe(url: &str, method: Method, timeout: Duration) -> Probe {
    let client = match Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            return Probe {
                reachable: false,
                status: None,
                latency_ms: 0,
                error: Some(format!("http client init: {e}")),
            }
        }
    };
    let start = Instant::now();
    match client.request(method, url).send() {
        Ok(resp) => Probe {
            reachable: true,
            status: Some(resp.status().as_u16()),
            latency_ms: start.elapsed().as_millis(),
            error: None,
        },
        Err(e) => Probe {
            reachable: false,
            status: None,
            latency_ms: start.elapsed().as_millis(),
            error: Some(short_error(&e)),
        },
    }
}

/// 把 reqwest 错误压成一行可读信息。
fn short_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        "connect refused/unreachable".into()
    } else {
        e.to_string()
    }
}
