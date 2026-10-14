//! 轻量计数器：`/metrics` 纯文本输出（prometheus 文本风格，无外部依赖）。
//! 口径见 gateway.md §4：请求/失败/failover/WS upgrade 次数，纯文本即可。

use std::collections::BTreeMap;
use std::sync::Mutex;

/// 计数器集合；key 为完整指标行标识（含 label），渲染按 key 排序输出。
#[derive(Debug, Default)]
pub struct Metrics {
    counters: Mutex<BTreeMap<String, u64>>,
}

impl Metrics {
    /// 某池的计数 +1：`inc("gateway_requests_total{pool=\"llm\"}")`。
    pub fn inc(&self, key: &str) {
        let mut m = self.counters.lock().unwrap();
        *m.entry(key.to_string()).or_insert(0) += 1;
    }

    pub fn inc_requests(&self, pool: &str) {
        self.inc(&format!("gateway_requests_total{{pool=\"{pool}\"}}"));
    }

    pub fn inc_upstream_failures(&self, pool: &str) {
        self.inc(&format!(
            "gateway_upstream_failures_total{{pool=\"{pool}\"}}"
        ));
    }

    pub fn inc_failovers(&self, pool: &str) {
        self.inc(&format!("gateway_failovers_total{{pool=\"{pool}\"}}"));
    }

    pub fn inc_backend_attempt(&self, backend: &str) {
        self.inc(&format!(
            "gateway_backend_attempts_total{{backend=\"{backend}\"}}"
        ));
    }

    pub fn inc_ws_upgrades(&self, pool: &str) {
        self.inc(&format!("gateway_ws_upgrades_total{{pool=\"{pool}\"}}"));
    }

    /// `GET /metrics` 文本体：`metric{labels} value` 逐行，按 key 排序。
    pub fn render(&self) -> String {
        let m = self.counters.lock().unwrap();
        let mut out = String::new();
        for (k, v) in m.iter() {
            out.push_str(k);
            out.push(' ');
            out.push_str(&v.to_string());
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_sorted_lines() {
        let m = Metrics::default();
        m.inc_failovers("llm");
        m.inc_requests("llm");
        m.inc_requests("llm");
        m.inc_backend_attempt("b1");
        let text = m.render();
        assert_eq!(
            text,
            "gateway_backend_attempts_total{backend=\"b1\"} 1\n\
             gateway_failovers_total{pool=\"llm\"} 1\n\
             gateway_requests_total{pool=\"llm\"} 2\n"
        );
    }
}
