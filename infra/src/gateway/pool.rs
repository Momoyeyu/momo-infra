//! upstream 池：优先级分组 + 组内加权轮询 + 失败熔断。
//! 语义见 docs/01-architecture/gateway.md §3：
//! 候选排序 = priority 升序分组 → 组内未熔断者按 weight 展开轮转 →
//! 组内熔断者追加尾部（fail-open）。每次请求现场计算，熔断即时生效。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

/// 单个 upstream 的运行时健康状态；请求结果与后台探测共同更新。
#[derive(Debug, Default)]
pub struct Health {
    pub consecutive_failures: u32,
    /// 熔断截止时刻；`Some` 且已过点视为未熔断（惰性过期）。
    pub cooling_until: Option<Instant>,
    /// 最近一次后台探测结果；未探测过为 None。
    pub last_probe_ok: Option<bool>,
    pub last_error: Option<String>,
}

#[derive(Debug)]
pub struct Backend {
    pub name: String,
    /// base URL，无尾斜杠；转发时拼客户端原始 path+query。
    pub endpoint: String,
    pub priority: i64,
    pub weight: u32,
    pub health_path: String,
    /// 非空则覆盖客户端 Authorization。
    pub api_key: Option<String>,
    /// 非空则改写请求 path（query 透传）。
    pub path_override: Option<String>,
    pub(crate) health: Mutex<Health>,
}

/// `/readyz` 用的 backend 快照。
#[derive(Debug, Serialize)]
pub struct BackendStatus {
    pub name: String,
    pub endpoint: String,
    pub priority: i64,
    pub weight: u32,
    pub cooling: bool,
    /// 冷却剩余毫秒（未熔断为 0）。
    pub cooling_remaining_ms: u64,
    pub consecutive_failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probe_ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Backend {
    pub fn is_cooling(&self) -> bool {
        self.health
            .lock()
            .unwrap()
            .cooling_until
            .is_some_and(|t| t > Instant::now())
    }

    /// 成功一次即清零失败计数并解除熔断。
    pub fn on_success(&self) {
        let mut h = self.health.lock().unwrap();
        h.consecutive_failures = 0;
        h.cooling_until = None;
        h.last_error = None;
    }

    /// 失败计数 +1；达到 `threshold` 置冷却 `cooldown` 时长。返回是否因此进入熔断。
    pub fn on_failure(&self, threshold: u32, cooldown: Duration, err: &str) -> bool {
        let mut h = self.health.lock().unwrap();
        h.consecutive_failures += 1;
        h.last_error = Some(err.to_string());
        if h.consecutive_failures >= threshold {
            h.cooling_until = Some(Instant::now() + cooldown);
            true
        } else {
            false
        }
    }

    /// 后台探测结果：成功等价 on_success（解除熔断）；
    /// 失败按 on_failure 累计（同样可能触发熔断）。
    pub fn on_probe(&self, ok: bool, threshold: u32, cooldown: Duration, err: Option<String>) {
        if ok {
            self.on_success();
        } else {
            self.on_failure(
                threshold,
                cooldown,
                err.as_deref().unwrap_or("probe failed"),
            );
        }
        self.health.lock().unwrap().last_probe_ok = Some(ok);
    }

    pub fn status(&self) -> BackendStatus {
        let h = self.health.lock().unwrap();
        let remaining = h
            .cooling_until
            .and_then(|t| t.checked_duration_since(Instant::now()))
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        BackendStatus {
            name: self.name.clone(),
            endpoint: self.endpoint.clone(),
            priority: self.priority,
            weight: self.weight,
            cooling: remaining > 0,
            cooling_remaining_ms: remaining,
            consecutive_failures: h.consecutive_failures,
            last_probe_ok: h.last_probe_ok,
            last_error: h.last_error.clone(),
        }
    }
}

/// 同一用途的 upstream 集合（llm / asr / tts 各一个实例）。
#[derive(Debug)]
pub struct Pool {
    /// 指标标签名："llm" | "asr" | "tts"。
    pub kind: &'static str,
    pub backends: Vec<Arc<Backend>>,
    pub cooldown_threshold: u32,
    pub cooldown: Duration,
    /// 组内加权轮询游标（跨请求递增）。
    rr: AtomicUsize,
}

impl Pool {
    pub fn new(
        kind: &'static str,
        backends: Vec<Arc<Backend>>,
        cooldown_threshold: u32,
        cooldown: Duration,
    ) -> Self {
        Self {
            kind,
            backends,
            cooldown_threshold,
            cooldown,
            rr: AtomicUsize::new(0),
        }
    }

    /// 单请求尝试序列（含 failover 次序），调用方按 `max_attempts` 截断。
    ///
    /// priority 升序分组；组内未熔断 backend 按 weight 展开后以轮询游标
    /// 轮转取序（去重，weight 只影响首选中率）；组内熔断者追加尾部做
    /// fail-open 兜底——全组熔断时仍可尝试，避免健康状态滞后全拒。
    pub fn attempt_order(&self) -> Vec<Arc<Backend>> {
        let mut groups: BTreeMap<i64, Vec<&Arc<Backend>>> = BTreeMap::new();
        for b in &self.backends {
            groups.entry(b.priority).or_default().push(b);
        }
        let mut out: Vec<Arc<Backend>> = Vec::with_capacity(self.backends.len());
        for members in groups.values() {
            // 未熔断成员的 weight 展开列表（保持配置序，weight 次出现）。
            let mut expanded: Vec<&Arc<Backend>> = vec![];
            for b in members.iter().filter(|b| !b.is_cooling()) {
                for _ in 0..b.weight.max(1) {
                    expanded.push(b);
                }
            }
            if !expanded.is_empty() {
                let start = self.rr.fetch_add(1, Ordering::Relaxed) % expanded.len();
                let mut seen = std::collections::BTreeSet::new();
                for i in 0..expanded.len() {
                    let b = expanded[(start + i) % expanded.len()];
                    if seen.insert(b.name.as_str()) {
                        out.push(b.clone());
                    }
                }
            }
            // 熔断中的成员按配置序追加（fail-open）。
            for b in members.iter().filter(|b| b.is_cooling()) {
                out.push((*b).clone());
            }
        }
        out
    }

    /// `/readyz`：池内有成员且全部熔断 → 该池 down。
    pub fn is_degraded(&self) -> bool {
        !self.backends.is_empty() && self.backends.iter().all(|b| b.is_cooling())
    }

    pub fn statuses(&self) -> Vec<BackendStatus> {
        self.backends.iter().map(|b| b.status()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(name: &str, priority: i64, weight: u32) -> Arc<Backend> {
        Arc::new(Backend {
            name: name.into(),
            endpoint: format!("http://{name}"),
            priority,
            weight,
            health_path: "/healthz".into(),
            api_key: None,
            path_override: None,
            health: Mutex::new(Health::default()),
        })
    }

    fn names(order: &[Arc<Backend>]) -> Vec<&str> {
        order.iter().map(|b| b.name.as_str()).collect()
    }

    /// 同优先级等权：轮询游标逐次轮转首选项。
    #[test]
    fn round_robin_rotates_first_pick() {
        let pool = Pool::new(
            "llm",
            vec![backend("a", 0, 1), backend("b", 0, 1)],
            3,
            Duration::from_secs(1),
        );
        assert_eq!(names(&pool.attempt_order()), ["a", "b"]);
        assert_eq!(names(&pool.attempt_order()), ["b", "a"]);
        assert_eq!(names(&pool.attempt_order()), ["a", "b"]);
    }

    /// 权重 3:1 → 4 次请求首选分布 3:1。
    #[test]
    fn weight_scales_first_pick_share() {
        let pool = Pool::new(
            "llm",
            vec![backend("a", 0, 3), backend("b", 0, 1)],
            3,
            Duration::from_secs(1),
        );
        let mut count = std::collections::BTreeMap::new();
        for _ in 0..4 {
            *count
                .entry(pool.attempt_order()[0].name.clone())
                .or_insert(0) += 1;
        }
        assert_eq!(count["a"], 3);
        assert_eq!(count["b"], 1);
    }

    /// 高 priority 组整体排后；低组全熔断后高组才进候选前列。
    #[test]
    fn priority_groups_order_low_first() {
        let pool = Pool::new(
            "llm",
            vec![backend("hi", 0, 1), backend("lo", 1, 1)],
            3,
            Duration::from_secs(1),
        );
        assert_eq!(names(&pool.attempt_order()), ["hi", "lo"]);
    }

    /// 熔断成员退出正常轮转、追加尾部；阈值内连续失败触发熔断。
    #[test]
    fn cooling_member_drops_to_tail() {
        let a = backend("a", 0, 1);
        let b = backend("b", 0, 1);
        let pool = Pool::new("llm", vec![a.clone(), b], 2, Duration::from_secs(60));
        a.on_failure(2, Duration::from_secs(60), "boom");
        assert!(!a.is_cooling());
        assert!(a.on_failure(2, Duration::from_secs(60), "boom"));
        assert!(a.is_cooling());
        // a 熔断 → 排到 b 之后（fail-open 仍在序列尾部）。
        assert_eq!(names(&pool.attempt_order()), ["b", "a"]);
        assert!(!pool.is_degraded());
        // 成功恢复：清计数、解除熔断。
        a.on_success();
        assert!(!a.is_cooling());
        assert_eq!(a.status().consecutive_failures, 0);
    }

    /// 单成员池熔断 → is_degraded；探活成功解除。
    #[test]
    fn degraded_pool_recovers_on_probe() {
        let a = backend("a", 0, 1);
        let pool = Pool::new("llm", vec![a.clone()], 1, Duration::from_secs(60));
        a.on_probe(false, 1, Duration::from_secs(60), Some("down".into()));
        assert!(pool.is_degraded());
        a.on_probe(true, 1, Duration::from_secs(60), None);
        assert!(!pool.is_degraded());
        assert_eq!(a.status().last_probe_ok, Some(true));
    }
}
