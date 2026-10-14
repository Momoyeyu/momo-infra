//! 后台健康探测 + `/readyz` 聚合。
//! 每 `health_interval_ms` 对每个 upstream `GET endpoint+health_path`：
//! 2xx 记成功（解除熔断），其余按失败累计（可触发熔断）。
//! 语义见 gateway.md §3.3。

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use super::pool::{Backend, BackendStatus};
use super::GatewayState;

/// `/readyz` 聚合快照。
#[derive(Debug, Serialize)]
pub struct ReadyReport {
    pub status: &'static str,
    pub pools: std::collections::BTreeMap<&'static str, PoolReport>,
}

#[derive(Debug, Serialize)]
pub struct PoolReport {
    /// 池内有成员且全部熔断。
    pub degraded: bool,
    pub backends: Vec<BackendStatus>,
}

/// ready 判定：配置了 upstream 的池全员熔断 → not ready（gateway.md §4）。
pub fn ready_report(state: &GatewayState) -> ReadyReport {
    let mut pools = std::collections::BTreeMap::new();
    for pool in [&state.llm, &state.asr, &state.tts] {
        pools.insert(
            pool.kind,
            PoolReport {
                degraded: pool.is_degraded(),
                backends: pool.statuses(),
            },
        );
    }
    let ready = pools.values().all(|p| !p.degraded);
    ReadyReport {
        status: if ready { "ready" } else { "not_ready" },
        pools,
    }
}

/// 启动后台探测任务（每 backend 一个循环任务，随进程退出）。
/// `health_interval_ms = 0` 不启动；无 health_path 语义的池仍按 `/healthz` 探。
pub fn spawn_probes(state: &Arc<GatewayState>) {
    let interval = state.cfg.health_interval_ms;
    if interval == 0 {
        return;
    }
    let interval = Duration::from_millis(interval);
    for pool in [&state.llm, &state.asr, &state.tts] {
        for backend in &pool.backends {
            let state = state.clone();
            let backend = backend.clone();
            let threshold = pool.cooldown_threshold;
            let cooldown = pool.cooldown;
            tokio::spawn(async move {
                probe_loop(state, backend, threshold, cooldown, interval).await;
            });
        }
    }
}

async fn probe_loop(
    state: Arc<GatewayState>,
    backend: Arc<Backend>,
    threshold: u32,
    cooldown: Duration,
    interval: Duration,
) {
    // 探测超时取请求超时与周期间的较小值，避免探针拖垮故障后端。
    let timeout = Duration::from_millis(state.cfg.request_timeout_ms).min(interval);
    let url = format!("{}{}", backend.endpoint, backend.health_path);
    loop {
        tokio::time::sleep(interval).await;
        let mut req = state.client.get(&url);
        if let Some(key) = &backend.api_key {
            let value = if key.contains(' ') {
                key.clone()
            } else {
                format!("Bearer {key}")
            };
            req = req.header(axum::http::header::AUTHORIZATION, value);
        }
        let outcome = tokio::time::timeout(timeout, req.send()).await;
        match outcome {
            Ok(Ok(resp)) => {
                let ok = resp.status().is_success();
                backend.on_probe(
                    ok,
                    threshold,
                    cooldown,
                    (!ok).then(|| format!("probe {}", resp.status())),
                );
            }
            Ok(Err(e)) => backend.on_probe(false, threshold, cooldown, Some(e.to_string())),
            Err(_) => {
                backend.on_probe(false, threshold, cooldown, Some("probe timeout".into()));
            }
        }
    }
}
