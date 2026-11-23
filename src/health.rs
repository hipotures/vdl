//! HomeStack health (`/health/live`, `/health/ready`) and service telemetry
//! (`/.well-known/homestack/status`, schema 3) with a real loopback heartbeat.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub const DEFAULT_HEARTBEAT_SECONDS: u64 = 60;

pub struct Heartbeat {
    started_at: Instant,
    last_beat_ms: AtomicU64,
    count: AtomicU64,
    interval_secs: u64,
}

impl Heartbeat {
    pub fn from_env() -> Self {
        let interval = std::env::var("HOMESTACK_HEARTBEAT_SECONDS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(DEFAULT_HEARTBEAT_SECONDS);
        Self::new(interval)
    }

    pub fn new(interval_secs: u64) -> Self {
        Self {
            started_at: Instant::now(),
            last_beat_ms: AtomicU64::new(0),
            count: AtomicU64::new(0),
            interval_secs: interval_secs.max(1),
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }

    pub fn record_beat(&self) {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        self.last_beat_ms.store(elapsed_ms, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn age_seconds(&self) -> u64 {
        let now_ms = self.started_at.elapsed().as_millis() as u64;
        let last_ms = self.last_beat_ms.load(Ordering::Relaxed);
        now_ms.saturating_sub(last_ms) / 1000
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn is_fresh(&self) -> bool {
        self.age_seconds() <= self.interval_secs.saturating_mul(3)
    }
}

/// Read `VmRSS` in bytes from `/proc/self/status` on demand.
pub fn process_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            let bytes = kb.checked_mul(1024)?;
            return if bytes > 0 { Some(bytes) } else { None };
        }
    }
    None
}

fn health_response(status: StatusCode, body: serde_json::Value) -> Response {
    let mut resp = (status, body.to_string()).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/health+json"),
    );
    resp
}

pub fn live_response(hb: &Heartbeat) -> Response {
    let fresh = hb.is_fresh();
    let status_str = if fresh { "pass" } else { "fail" };
    let code = if fresh {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    health_response(
        code,
        json!({
            "status": status_str,
            "checks": {
                "heartbeat:age": [{
                    "status": status_str,
                    "observedValue": hb.age_seconds(),
                    "observedUnit": "s",
                }],
                "heartbeat:count": [{
                    "status": status_str,
                    "observedValue": hb.count(),
                }],
            }
        }),
    )
}

pub fn ready_response(hb: &Heartbeat, download_root: &Path) -> Response {
    let fresh = hb.is_fresh();
    let hb_status = if fresh { "pass" } else { "fail" };
    let fs_ok = fs::create_dir_all(download_root).is_ok() && fs::read_dir(download_root).is_ok();
    let fs_status = if fs_ok { "pass" } else { "fail" };
    let all_ok = fresh && fs_ok;
    let overall = if all_ok { "pass" } else { "fail" };
    let code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    health_response(
        code,
        json!({
            "status": overall,
            "checks": {
                "heartbeat:age": [{
                    "status": hb_status,
                    "observedValue": hb.age_seconds(),
                    "observedUnit": "s",
                }],
                "heartbeat:count": [{
                    "status": hb_status,
                    "observedValue": hb.count(),
                }],
                "storage:download_root": [{
                    "status": fs_status,
                }],
            }
        }),
    )
}

pub fn telemetry_response(hb: &Heartbeat) -> Response {
    let status_str = if hb.is_fresh() { "pass" } else { "fail" };
    let body = json!({
        "schema": 3,
        "status": status_str,
        "process_rss_bytes": process_rss_bytes(),
    });
    let mut resp = (StatusCode::OK, body.to_string()).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Spawn the real loopback HTTP heartbeat against `GET /api/state`.
pub fn spawn_heartbeat(hb: Arc<Heartbeat>, port: u16) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        let url = format!("http://127.0.0.1:{port}/api/state");
        // Initial beat shortly after bind, then every interval.
        tokio::time::sleep(Duration::from_millis(200)).await;
        loop {
            if let Ok(resp) = client.get(&url).send().await
                && resp.status().is_success()
            {
                hb.record_beat();
            }
            tokio::time::sleep(hb.interval()).await;
        }
    })
}
