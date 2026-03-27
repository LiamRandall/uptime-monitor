//! # Uptime Monitor
//!
//! A wasmCloud component that monitors network device uptime by polling HTTP
//! health-check endpoints and serving the collected metrics through a REST API.
//!
//! ## Architecture
//!
//! The component uses three WASI capabilities:
//!
//! - **`wasi:http/incoming-handler`** — serves the REST API so you can query
//!   metrics, add/remove devices, and trigger manual polls.
//! - **`wasi:http/outgoing-handler`** — makes HTTP requests to each monitored
//!   device to check whether it is reachable.
//! - **`wasi:keyvalue/store`** — persists device configuration and metric
//!   snapshots across invocations so state survives restarts.
//!
//! ## Polling
//!
//! A companion service component calls the exported `cron` interface every
//! 1 second to poll all devices, and every 60 seconds to prune old history.
//!
//! ## REST Endpoints
//!
//! | Method   | Path                          | Description                         |
//! |----------|-------------------------------|-------------------------------------|
//! | `GET`    | `/health`                     | Component health check              |
//! | `GET`    | `/api/devices`                | List all monitored devices          |
//! | `POST`   | `/api/devices`                | Register a new device               |
//! | `DELETE` | `/api/devices/{id}`           | Remove a device                     |
//! | `GET`    | `/api/metrics`                | Metrics for every device            |
//! | `GET`    | `/api/metrics/{id}`           | Metrics for a single device         |
//! | `POST`   | `/api/poll`                   | Trigger an immediate poll of all    |
//! | `POST`   | `/api/poll/{id}`              | Trigger a poll of one device        |
//! | `GET`    | `/api/dashboard`              | Main HTML dashboard                 |
//! | `GET`    | `/api/dashboard/{id}`         | Detailed page for a single device   |
//! | `GET`    | `/api/history/{id}`           | Raw check history (JSON)            |

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "cron")]
mod bindings {
    wit_bindgen::generate!({
        world: "uptime-monitor",
        path: "../wit",
        generate_all,
    });
}

#[cfg(not(feature = "cron"))]
mod bindings {
    wit_bindgen::generate!({
        world: "uptime-monitor-standalone",
        path: "../wit",
        generate_all,
    });
}

#[cfg(feature = "cron")]
use bindings::exports::cosmonic::uptime_monitor::cron::Guest as CronGuest;
use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::outgoing_handler;
use bindings::wasi::http::types::{
    Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingRequest,
    OutgoingResponse, ResponseOutparam, Scheme,
};
use bindings::wasi::keyvalue::store;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// A network device to monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Device {
    /// Unique identifier (slug-style, e.g. "living-room-router").
    id: String,
    /// Human-readable label.
    name: String,
    /// Full URL to poll, e.g. `http://192.168.1.1/status`.
    url: String,
    /// Expected HTTP status code that means "up" (default 200).
    #[serde(default = "default_expected_status")]
    expected_status: u16,
}

fn default_expected_status() -> u16 {
    200
}

/// The body accepted when registering a new device via `POST /api/devices`.
#[derive(Debug, Deserialize)]
struct CreateDeviceRequest {
    id: String,
    name: String,
    url: String,
    #[serde(default = "default_expected_status")]
    expected_status: u16,
}

/// Recorded metrics for a single device.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceMetrics {
    device_id: String,
    is_up: bool,
    last_status_code: Option<u16>,
    last_checked: String,
    response_time_ms: Option<u64>,
    total_checks: u64,
    total_up: u64,
    total_down: u64,
    uptime_percentage: f64,
    last_error: Option<String>,
    consecutive_failures: u32,
}

impl DeviceMetrics {
    fn new(device_id: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            is_up: false,
            last_status_code: None,
            last_checked: String::from("never"),
            response_time_ms: None,
            total_checks: 0,
            total_up: 0,
            total_down: 0,
            uptime_percentage: 0.0,
            last_error: None,
            consecutive_failures: 0,
        }
    }

    fn record_success(&mut self, status_code: u16, response_time_ms: u64, timestamp: &str) {
        self.is_up = true;
        self.last_status_code = Some(status_code);
        self.last_checked = timestamp.to_string();
        self.response_time_ms = Some(response_time_ms);
        self.total_checks += 1;
        self.total_up += 1;
        self.last_error = None;
        self.consecutive_failures = 0;
        self.recalculate_uptime();
    }

    fn record_failure(&mut self, error: &str, timestamp: &str) {
        self.is_up = false;
        self.last_status_code = None;
        self.last_checked = timestamp.to_string();
        self.response_time_ms = None;
        self.total_checks += 1;
        self.total_down += 1;
        self.last_error = Some(error.to_string());
        self.consecutive_failures += 1;
        self.recalculate_uptime();
    }

    fn record_unexpected_status(
        &mut self,
        status_code: u16,
        expected: u16,
        response_time_ms: u64,
        timestamp: &str,
    ) {
        self.is_up = false;
        self.last_status_code = Some(status_code);
        self.last_checked = timestamp.to_string();
        self.response_time_ms = Some(response_time_ms);
        self.total_checks += 1;
        self.total_down += 1;
        self.last_error = Some(format!(
            "unexpected status: got {status_code}, expected {expected}"
        ));
        self.consecutive_failures += 1;
        self.recalculate_uptime();
    }

    fn recalculate_uptime(&mut self) {
        if self.total_checks > 0 {
            self.uptime_percentage =
                (self.total_up as f64 / self.total_checks as f64) * 100.0;
        }
    }
}

/// A single check result, stored in the history for 24h status bars.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckResult {
    /// Unix epoch seconds when the check occurred.
    epoch_secs: u64,
    /// ISO timestamp string.
    timestamp: String,
    /// Whether the check was successful.
    is_up: bool,
    /// HTTP status code received (if any).
    status_code: Option<u16>,
    /// Response time in milliseconds (if any).
    response_time_ms: Option<u64>,
    /// Error message (if failed).
    error: Option<String>,
}

/// Summary data for one device row on the main dashboard (JSON API).
#[derive(Serialize)]
struct DashboardDevice {
    id: String,
    name: String,
    url: String,
    is_up: bool,
    uptime_percentage: f64,
    response_time_ms: Option<u64>,
    last_checked: String,
    /// 288 five-minute buckets for the 24h bar: null=no data, true=up, false=down
    bar: Vec<Option<bool>>,
}

/// Detailed data for a single device page (JSON API).
#[derive(Serialize)]
struct DashboardDetail {
    id: String,
    name: String,
    url: String,
    is_up: bool,
    uptime_24h: f64,
    uptime_all: f64,
    avg_response_ms: Option<u64>,
    checks_24h: u64,
    down_24h: u64,
    consecutive_failures: u32,
    total_checks: u64,
    bar: Vec<Option<bool>>,
    recent_checks: Vec<CheckResult>,
}

// ---------------------------------------------------------------------------
// Key-value helpers
// ---------------------------------------------------------------------------

/// The keyvalue backend to use. The host runtime selects the actual backend
/// based on `.wash/config.yaml`:
///
/// | `KV_BACKEND`   | Required config                              |
/// |----------------|----------------------------------------------|
/// | `"in_memory"`  | none (default)                               |
/// | `"redis"`      | `wasi_keyvalue_redis_url: redis://...`        |
const KV_BACKEND: &str = "in_memory";

const KV_DEVICES_KEY: &str = "uptime:devices";
const KV_METRICS_PREFIX: &str = "uptime:metrics:";
const KV_HISTORY_PREFIX: &str = "uptime:history:";

const HISTORY_RETENTION_SECS: u64 = 24 * 60 * 60; // 24 hours

/// Load the device list from the KV store.
fn load_devices() -> HashMap<String, Device> {
    let bucket = match store::open(KV_BACKEND) {
        Ok(b) => b,
        Err(_) => return HashMap::new(),
    };
    match bucket.get(KV_DEVICES_KEY) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
        _ => HashMap::new(),
    }
}

/// Persist the device list.
fn save_devices(devices: &HashMap<String, Device>) {
    let Ok(bucket) = store::open(KV_BACKEND) else {
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(devices) {
        let _ = bucket.set(KV_DEVICES_KEY, &bytes);
    }
}

/// Load metrics for a device.
fn load_metrics(device_id: &str) -> DeviceMetrics {
    let key = format!("{KV_METRICS_PREFIX}{device_id}");
    let bucket = match store::open(KV_BACKEND) {
        Ok(b) => b,
        Err(_) => return DeviceMetrics::new(device_id),
    };
    match bucket.get(&key) {
        Ok(Some(bytes)) => {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| DeviceMetrics::new(device_id))
        }
        _ => DeviceMetrics::new(device_id),
    }
}

/// Persist metrics for a device.
fn save_metrics(metrics: &DeviceMetrics) {
    let key = format!("{KV_METRICS_PREFIX}{}", metrics.device_id);
    let Ok(bucket) = store::open(KV_BACKEND) else {
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(metrics) {
        let _ = bucket.set(&key, &bytes);
    }
}

/// Delete metrics for a device.
fn delete_metrics(device_id: &str) {
    let key = format!("{KV_METRICS_PREFIX}{device_id}");
    let Ok(bucket) = store::open(KV_BACKEND) else {
        return;
    };
    let _ = bucket.delete(&key);
}

/// Load check history for a device.
fn load_history(device_id: &str) -> Vec<CheckResult> {
    let key = format!("{KV_HISTORY_PREFIX}{device_id}");
    let bucket = match store::open(KV_BACKEND) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    match bucket.get(&key) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Save check history for a device.
fn save_history(device_id: &str, history: &[CheckResult]) {
    let key = format!("{KV_HISTORY_PREFIX}{device_id}");
    let Ok(bucket) = store::open(KV_BACKEND) else {
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(history) {
        let _ = bucket.set(&key, &bytes);
    }
}

/// Delete check history for a device.
fn delete_history(device_id: &str) {
    let key = format!("{KV_HISTORY_PREFIX}{device_id}");
    let Ok(bucket) = store::open(KV_BACKEND) else {
        return;
    };
    let _ = bucket.delete(&key);
}

/// Append a check result and prune entries older than 24h.
fn append_and_prune_history(device_id: &str, result: CheckResult) {
    let mut history = load_history(device_id);
    let cutoff = now_epoch_secs().saturating_sub(HISTORY_RETENTION_SECS);
    history.retain(|r| r.epoch_secs >= cutoff);
    history.push(result);
    save_history(device_id, &history);
}

/// Prune all device histories (called on each poll).
fn prune_all_histories(devices: &HashMap<String, Device>) {
    let cutoff = now_epoch_secs().saturating_sub(HISTORY_RETENTION_SECS);
    for device_id in devices.keys() {
        let mut history = load_history(device_id);
        let before = history.len();
        history.retain(|r| r.epoch_secs >= cutoff);
        if history.len() != before {
            save_history(device_id, &history);
        }
    }
}

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------

/// Current time as Unix epoch seconds.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Get a rough timestamp string.
fn now_timestamp() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            let days = secs / 86400;
            let remaining = secs % 86400;
            let hours = remaining / 3600;
            let minutes = (remaining % 3600) / 60;
            let seconds = remaining % 60;
            let (year, month, day) = epoch_days_to_date(days);
            format!(
                "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z"
            )
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Convert days since Unix epoch to (year, month, day).
fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

// ---------------------------------------------------------------------------
// HTTP polling
// ---------------------------------------------------------------------------

/// Poll a single device by making an outgoing HTTP request.
fn poll_device(device: &Device) -> DeviceMetrics {
    let mut metrics = load_metrics(&device.id);
    let timestamp = now_timestamp();
    let epoch = now_epoch_secs();

    let start = std::time::Instant::now();

    let (scheme, authority, path) = match parse_url(&device.url) {
        Some(parts) => parts,
        None => {
            metrics.record_failure(&format!("invalid URL: {}", device.url), &timestamp);
            save_metrics(&metrics);
            append_and_prune_history(&device.id, CheckResult {
                epoch_secs: epoch,
                timestamp: timestamp.clone(),
                is_up: false,
                status_code: None,
                response_time_ms: None,
                error: Some(format!("invalid URL: {}", device.url)),
            });
            return metrics;
        }
    };

    let headers = Fields::new();
    let request = OutgoingRequest::new(headers);
    request.set_method(&Method::Get).ok();
    request.set_scheme(Some(&scheme)).ok();
    request.set_authority(Some(&authority)).ok();
    request.set_path_with_query(Some(&path)).ok();

    match outgoing_handler::handle(request, None) {
        Ok(future_response) => {
            let response = match future_response.get() {
                Some(Ok(Ok(resp))) => resp,
                Some(Ok(Err(e))) => {
                    let err = format!("HTTP error: {e:?}");
                    metrics.record_failure(&err, &timestamp);
                    save_metrics(&metrics);
                    append_and_prune_history(&device.id, CheckResult {
                        epoch_secs: epoch,
                        timestamp,
                        is_up: false,
                        status_code: None,
                        response_time_ms: None,
                        error: Some(err),
                    });
                    return metrics;
                }
                Some(Err(())) => {
                    let err = "response future error";
                    metrics.record_failure(err, &timestamp);
                    save_metrics(&metrics);
                    append_and_prune_history(&device.id, CheckResult {
                        epoch_secs: epoch,
                        timestamp,
                        is_up: false,
                        status_code: None,
                        response_time_ms: None,
                        error: Some(err.to_string()),
                    });
                    return metrics;
                }
                None => {
                    let pollable = future_response.subscribe();
                    pollable.block();
                    match future_response.get() {
                        Some(Ok(Ok(resp))) => resp,
                        _ => {
                            let err = "timeout waiting for response";
                            metrics.record_failure(err, &timestamp);
                            save_metrics(&metrics);
                            append_and_prune_history(&device.id, CheckResult {
                                epoch_secs: epoch,
                                timestamp,
                                is_up: false,
                                status_code: None,
                                response_time_ms: None,
                                error: Some(err.to_string()),
                            });
                            return metrics;
                        }
                    }
                }
            };

            let elapsed_ms = start.elapsed().as_millis() as u64;
            let status = response.status();

            if status == device.expected_status {
                metrics.record_success(status, elapsed_ms, &timestamp);
                append_and_prune_history(&device.id, CheckResult {
                    epoch_secs: epoch,
                    timestamp,
                    is_up: true,
                    status_code: Some(status),
                    response_time_ms: Some(elapsed_ms),
                    error: None,
                });
            } else {
                metrics.record_unexpected_status(status, device.expected_status, elapsed_ms, &timestamp);
                append_and_prune_history(&device.id, CheckResult {
                    epoch_secs: epoch,
                    timestamp,
                    is_up: false,
                    status_code: Some(status),
                    response_time_ms: Some(elapsed_ms),
                    error: Some(format!("expected {}, got {status}", device.expected_status)),
                });
            }
        }
        Err(e) => {
            let err = format!("request failed: {e:?}");
            metrics.record_failure(&err, &timestamp);
            append_and_prune_history(&device.id, CheckResult {
                epoch_secs: epoch,
                timestamp,
                is_up: false,
                status_code: None,
                response_time_ms: None,
                error: Some(err),
            });
        }
    }

    save_metrics(&metrics);
    metrics
}

/// Minimal URL parser — returns (Scheme, authority, path_with_query).
fn parse_url(url: &str) -> Option<(Scheme, String, String)> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else {
        return None;
    };

    let (authority, path) = match rest.find('/') {
        Some(idx) => (rest[..idx].to_string(), rest[idx..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    };

    Some((scheme, authority, path))
}

// ---------------------------------------------------------------------------
// HTTP response helpers
// ---------------------------------------------------------------------------

fn send_response(outparam: ResponseOutparam, status: u16, content_type: &str, body_bytes: &[u8]) {
    let headers = Fields::new();
    let _ = headers.append(
        &"content-type".to_string(),
        &content_type.as_bytes().to_vec(),
    );
    let response = OutgoingResponse::new(headers);
    response.set_status_code(status).ok();
    let out_body = response.body().expect("failed to get outgoing body");
    ResponseOutparam::set(outparam, Ok(response));
    let stream = out_body.write().expect("failed to get write stream");
    // Write in chunks to avoid exceeding WASI stream buffer limits
    for chunk in body_bytes.chunks(4096) {
        let _ = stream.blocking_write_and_flush(chunk);
    }
    drop(stream);
    OutgoingBody::finish(out_body, None).expect("failed to finish body");
}

fn json_response(outparam: ResponseOutparam, status: u16, value: &impl Serialize) {
    let body = serde_json::to_vec_pretty(value).unwrap_or_default();
    send_response(outparam, status, "application/json", &body);
}

fn error_json(outparam: ResponseOutparam, status: u16, message: &str) {
    let body = serde_json::json!({ "error": message });
    json_response(outparam, status, &body);
}

// ---------------------------------------------------------------------------
// Request body reader
// ---------------------------------------------------------------------------

fn read_request_body(request: &IncomingRequest) -> Vec<u8> {
    let body = match request.consume() {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let stream = match body.stream() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut buf = Vec::new();
    loop {
        match stream.blocking_read(65536) {
            Ok(chunk) if chunk.is_empty() => break,
            Ok(chunk) => buf.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    drop(stream);
    IncomingBody::finish(body);
    buf
}

// ---------------------------------------------------------------------------
// Dashboard HTML — shared styles
// ---------------------------------------------------------------------------

const DASHBOARD_CSS: &str = r##"
  :root { --bg: #0f172a; --card: #1e293b; --border: #334155;
           --text: #e2e8f0; --muted: #94a3b8; --up: #22c55e; --down: #ef4444;
           --accent: #3b82f6; }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { background: var(--bg); color: var(--text); font-family: system-ui, sans-serif; padding: 2rem; }
  a { color: var(--accent); text-decoration: none; }
  a:hover { text-decoration: underline; }
  h1 { font-size: 1.5rem; margin-bottom: 0.25rem; }
  h1 .sub { color: var(--muted); font-weight: 400; font-size: 0.9rem; margin-left: 0.5rem; }
  .poll-status { color: var(--muted); font-size: 0.8rem; margin-bottom: 1rem; }
  table { width: 100%; border-collapse: collapse; background: var(--card); border-radius: 8px; overflow: hidden; }
  th, td { padding: 0.75rem 1rem; text-align: left; border-bottom: 1px solid var(--border); }
  th { background: var(--border); font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted); }
  .mono { font-family: "JetBrains Mono", monospace; font-size: 0.85rem; }
  .status-dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 6px; }
  .status-dot.up { background: var(--up); box-shadow: 0 0 6px var(--up); }
  .status-dot.down { background: var(--down); box-shadow: 0 0 6px var(--down); }
  .status.up { color: var(--up); font-weight: 600; }
  .status.down { color: var(--down); font-weight: 600; }
  .empty { text-align: center; padding: 3rem; color: var(--muted); }
  .actions { margin-bottom: 1rem; display: flex; gap: 0.75rem; align-items: center; }
  .btn { background: var(--accent); color: #fff; border: none; padding: 0.5rem 1rem;
         border-radius: 6px; cursor: pointer; font-size: 0.85rem; }
  .btn:hover { background: #2563eb; }
  .btn.sm { padding: 0.3rem 0.6rem; font-size: 0.75rem; }
  .uptime-bar { display: flex; height: 24px; border-radius: 4px; overflow: hidden; background: var(--border); width: 100%; min-width: 200px; }
  .uptime-bar .seg { min-width: 1px; }
  .uptime-bar .seg.up { background: var(--up); }
  .uptime-bar .seg.down { background: var(--down); }
  .uptime-bar .seg.gap { background: var(--border); }
  .card { background: var(--card); border-radius: 8px; padding: 1.5rem; margin-bottom: 1rem; border: 1px solid var(--border); }
  .stat-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 1rem; margin-bottom: 1.5rem; }
  .stat { background: var(--bg); border-radius: 6px; padding: 1rem; text-align: center; }
  .stat .label { font-size: 0.7rem; text-transform: uppercase; color: var(--muted); letter-spacing: 0.05em; margin-bottom: 0.25rem; }
  .stat .value { font-size: 1.4rem; font-weight: 700; }
  .stat .value.up { color: var(--up); }
  .stat .value.down { color: var(--down); }
  .history-table { margin-top: 1rem; }
  .history-table td { font-size: 0.85rem; padding: 0.4rem 0.75rem; }
  .detail-bar-container { margin: 1.5rem 0; }
  .detail-bar-container .uptime-bar { height: 40px; border-radius: 6px; }
  .detail-bar-labels { display: flex; justify-content: space-between; font-size: 0.7rem; color: var(--muted); margin-top: 0.25rem; }
"##;

// ---------------------------------------------------------------------------
// Dashboard — main page
// ---------------------------------------------------------------------------

fn render_dashboard(devices: &HashMap<String, Device>) -> String {
    let mut rows = String::new();
    let mut sorted_ids: Vec<&String> = devices.keys().collect();
    sorted_ids.sort();

    let now = now_epoch_secs();

    for id in &sorted_ids {
        let device = &devices[*id];
        let metrics = load_metrics(id);
        let history = load_history(id);
        let status_class = if metrics.is_up { "up" } else { "down" };
        let status_label = if metrics.is_up { "UP" } else { "DOWN" };
        let uptime = format!("{:.1}%", metrics.uptime_percentage);
        let response_time = metrics
            .response_time_ms
            .map_or(String::from("--"), |ms| format!("{ms} ms"));

        // Build 24h status bar: divide 24h into 288 segments of 5 minutes each
        let bar_html = render_24h_bar_html(&compute_24h_buckets(&history, now));

        rows.push_str(&format!(
            r#"<tr>
  <td><span class="status-dot {status_class}"></span> <a href="/api/dashboard/{id}">{name}</a></td>
  <td class="status {status_class}">{status_label}</td>
  <td>{uptime}</td>
  <td>{response_time}</td>
  <td>{bar_html}</td>
</tr>"#,
            id = id,
            name = device.name,
        ));
    }

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Uptime Monitor</title>
<style>{css}</style>
</head>
<body>
  <h1>Uptime Monitor <span class="sub">wasmCloud Component</span></h1>
  <div class="poll-status" id="poll-status">Service polling every 1s &middot; dashboard updating live</div>
  <div class="actions">
    <button class="btn" onclick="fetch('/api/poll',{{method:'POST'}}).then(()=>refresh())">Poll All Now</button>
  </div>
  <table>
    <thead>
      <tr>
        <th>Device</th><th>Status</th><th>Uptime</th>
        <th>Response</th><th>Last 24 Hours</th>
      </tr>
    </thead>
    <tbody id="device-rows">
      {rows}
      {empty}
    </tbody>
  </table>
  <script>
    function barHtml(buckets) {{
      return '<div class="uptime-bar" title="Last 24 hours">' +
        buckets.map(b => '<div class="seg ' +
          (b === null ? 'gap' : b ? 'up' : 'down') +
          '" style="flex:1"></div>').join('') + '</div>';
    }}
    function refresh() {{
      fetch('/api/dashboard-data')
        .then(r => r.json())
        .then(devices => {{
          const tbody = document.getElementById('device-rows');
          if (!devices.length) {{
            tbody.innerHTML = '<tr><td colspan="5" class="empty">No devices registered yet. POST to /api/devices to add one.</td></tr>';
            return;
          }}
          tbody.innerHTML = devices.map(d => `<tr>
            <td><span class="status-dot ${{d.is_up?'up':'down'}}"></span> <a href="/api/dashboard/${{d.id}}">${{d.name}}</a></td>
            <td class="status ${{d.is_up?'up':'down'}}">${{d.is_up?'UP':'DOWN'}}</td>
            <td>${{d.uptime_percentage.toFixed(1)}}%</td>
            <td>${{d.response_time_ms!=null?d.response_time_ms+' ms':'--'}}</td>
            <td>${{barHtml(d.bar)}}</td>
          </tr>`).join('');
        }})
        .catch(() => {{}});
    }}
    setInterval(refresh, 2000);
  </script>
</body>
</html>"##,
        css = DASHBOARD_CSS,
        rows = rows,
        empty = if rows.is_empty() {
            r#"<tr><td colspan="5" class="empty">No devices registered yet. POST to /api/devices to add one.</td></tr>"#
        } else {
            ""
        }
    )
}

/// Compute 24h status buckets from check history.
/// Returns 288 five-minute buckets: None=no data, Some(true)=up, Some(false)=down.
fn compute_24h_buckets(history: &[CheckResult], now_secs: u64) -> Vec<Option<bool>> {
    let window = HISTORY_RETENTION_SECS;
    let num_buckets = 288u64;
    let bucket_size = window / num_buckets;
    let start = now_secs.saturating_sub(window);

    let mut buckets: Vec<Option<bool>> = vec![None; num_buckets as usize];
    for check in history {
        if check.epoch_secs < start {
            continue;
        }
        let offset = check.epoch_secs - start;
        let idx = (offset / bucket_size).min(num_buckets - 1) as usize;
        buckets[idx] = Some(match buckets[idx] {
            None => check.is_up,
            Some(prev) => prev && check.is_up,
        });
    }
    buckets
}

/// Render a 24-hour status bar as HTML from precomputed buckets.
fn render_24h_bar_html(buckets: &[Option<bool>]) -> String {
    let mut html = String::from(r#"<div class="uptime-bar" title="Last 24 hours — each segment is 5 minutes">"#);
    for b in buckets {
        let class = match b {
            None => "seg gap",
            Some(true) => "seg up",
            Some(false) => "seg down",
        };
        html.push_str(&format!(
            r#"<div class="{class}" style="flex:1"></div>"#
        ));
    }
    html.push_str("</div>");
    html
}

// ---------------------------------------------------------------------------
// Dashboard — detail page for a single device
// ---------------------------------------------------------------------------

fn render_detail_page(device: &Device, metrics: &DeviceMetrics, history: &[CheckResult]) -> String {
    let now = now_epoch_secs();
    let status_class = if metrics.is_up { "up" } else { "down" };
    let status_label = if metrics.is_up { "UP" } else { "DOWN" };

    // Compute 24h stats from history
    let checks_24h = history.len();
    let up_24h = history.iter().filter(|c| c.is_up).count();
    let down_24h = checks_24h - up_24h;
    let uptime_24h = if checks_24h > 0 {
        (up_24h as f64 / checks_24h as f64) * 100.0
    } else {
        0.0
    };
    let avg_response = {
        let times: Vec<u64> = history.iter().filter_map(|c| c.response_time_ms).collect();
        if times.is_empty() {
            "--".to_string()
        } else {
            let sum: u64 = times.iter().sum();
            format!("{} ms", sum / times.len() as u64)
        }
    };

    let bar_html = render_24h_bar_html(&compute_24h_buckets(history, now));

    // Recent checks table (last 50, newest first)
    let mut recent: Vec<&CheckResult> = history.iter().collect();
    recent.sort_by(|a, b| b.epoch_secs.cmp(&a.epoch_secs));
    recent.truncate(50);

    let mut check_rows = String::new();
    for c in &recent {
        let cls = if c.is_up { "up" } else { "down" };
        let label = if c.is_up { "UP" } else { "DOWN" };
        let code = c.status_code.map_or("--".to_string(), |s| s.to_string());
        let rt = c.response_time_ms.map_or("--".to_string(), |ms| format!("{ms} ms"));
        let err = c.error.as_deref().unwrap_or("--");
        check_rows.push_str(&format!(
            r#"<tr>
  <td class="mono">{ts}</td>
  <td class="status {cls}">{label}</td>
  <td>{code}</td>
  <td>{rt}</td>
  <td class="mono" style="max-width:300px;overflow:hidden;text-overflow:ellipsis">{err}</td>
</tr>"#,
            ts = c.timestamp,
        ));
    }

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name} — Uptime Monitor</title>
<style>{css}</style>
</head>
<body>
  <p style="margin-bottom:1rem"><a href="/api/dashboard">&larr; Back to Dashboard</a></p>
  <h1><span class="status-dot {status_class}"></span> {name} <span class="sub">{status_label}</span></h1>
  <p class="mono" style="color:var(--muted);margin-bottom:1.5rem">{url}</p>

  <div class="stat-grid">
    <div class="stat">
      <div class="label">Current Status</div>
      <div class="value {status_class}">{status_label}</div>
    </div>
    <div class="stat">
      <div class="label">Uptime (24h)</div>
      <div class="value">{uptime_24h:.1}%</div>
    </div>
    <div class="stat">
      <div class="label">Uptime (all time)</div>
      <div class="value">{uptime_all:.1}%</div>
    </div>
    <div class="stat">
      <div class="label">Avg Response (24h)</div>
      <div class="value">{avg_response}</div>
    </div>
    <div class="stat">
      <div class="label">Checks (24h)</div>
      <div class="value">{checks_24h}</div>
    </div>
    <div class="stat">
      <div class="label">Failures (24h)</div>
      <div class="value {down_cls}">{down_24h}</div>
    </div>
    <div class="stat">
      <div class="label">Consec. Failures</div>
      <div class="value {consec_cls}">{consecutive}</div>
    </div>
    <div class="stat">
      <div class="label">Total Checks</div>
      <div class="value">{total_checks}</div>
    </div>
  </div>

  <div class="card">
    <h3 style="margin-bottom:0.5rem;font-size:0.9rem">Last 24 Hours</h3>
    <div class="detail-bar-container">
      <div id="detail-bar">{bar_html}</div>
      <div class="detail-bar-labels">
        <span>24h ago</span>
        <span>Now</span>
      </div>
    </div>
  </div>

  <div class="card">
    <h3 style="margin-bottom:0.5rem;font-size:0.9rem">Recent Checks</h3>
    <table class="history-table">
      <thead>
        <tr><th>Time</th><th>Status</th><th>Code</th><th>Response</th><th>Error</th></tr>
      </thead>
      <tbody id="checks-body">
        {check_rows}
        {empty_checks}
      </tbody>
    </table>
  </div>

  <script>
    const deviceId = '{id}';
    function barHtml(buckets) {{
      return '<div class="uptime-bar" title="Last 24 hours">' +
        buckets.map(b => '<div class="seg ' +
          (b === null ? 'gap' : b ? 'up' : 'down') +
          '" style="flex:1"></div>').join('') + '</div>';
    }}
    function refresh() {{
      fetch('/api/dashboard-data/' + deviceId)
        .then(r => r.json())
        .then(d => {{
          // Update status dot + label
          document.querySelector('h1 .status-dot').className = 'status-dot ' + (d.is_up ? 'up' : 'down');
          document.querySelector('h1 .sub').textContent = d.is_up ? 'UP' : 'DOWN';
          document.querySelector('h1 .sub').className = 'sub status ' + (d.is_up ? 'up' : 'down');
          // Update stat values
          const vals = document.querySelectorAll('.stat .value');
          vals[0].textContent = d.is_up ? 'UP' : 'DOWN';
          vals[0].className = 'value ' + (d.is_up ? 'up' : 'down');
          vals[1].textContent = d.uptime_24h.toFixed(1) + '%';
          vals[2].textContent = d.uptime_all.toFixed(1) + '%';
          vals[3].textContent = d.avg_response_ms != null ? d.avg_response_ms + ' ms' : '--';
          vals[4].textContent = d.checks_24h;
          vals[5].textContent = d.down_24h;
          vals[5].className = 'value' + (d.down_24h > 0 ? ' down' : '');
          vals[6].textContent = d.consecutive_failures;
          vals[6].className = 'value' + (d.consecutive_failures > 0 ? ' down' : '');
          vals[7].textContent = d.total_checks;
          // Update bar
          document.getElementById('detail-bar').innerHTML = barHtml(d.bar);
          // Update recent checks table
          const tbody = document.getElementById('checks-body');
          if (!d.recent_checks.length) {{
            tbody.innerHTML = '<tr><td colspan="5" class="empty">No checks recorded yet.</td></tr>';
          }} else {{
            tbody.innerHTML = d.recent_checks.map(c => `<tr>
              <td class="mono">${{c.timestamp}}</td>
              <td class="status ${{c.is_up?'up':'down'}}">${{c.is_up?'UP':'DOWN'}}</td>
              <td>${{c.status_code!=null?c.status_code:'--'}}</td>
              <td>${{c.response_time_ms!=null?c.response_time_ms+' ms':'--'}}</td>
              <td class="mono" style="max-width:300px;overflow:hidden;text-overflow:ellipsis">${{c.error||'--'}}</td>
            </tr>`).join('');
          }}
        }})
        .catch(() => {{}});
    }}
    setInterval(refresh, 2000);
  </script>
</body>
</html>"##,
        css = DASHBOARD_CSS,
        name = device.name,
        url = device.url,
        id = device.id,
        status_class = status_class,
        status_label = status_label,
        uptime_24h = uptime_24h,
        uptime_all = metrics.uptime_percentage,
        avg_response = avg_response,
        checks_24h = checks_24h,
        down_24h = down_24h,
        down_cls = if down_24h > 0 { "down" } else { "" },
        consecutive = metrics.consecutive_failures,
        consec_cls = if metrics.consecutive_failures > 0 { "down" } else { "" },
        total_checks = metrics.total_checks,
        bar_html = bar_html,
        check_rows = check_rows,
        empty_checks = if check_rows.is_empty() {
            r#"<tr><td colspan="5" class="empty">No checks recorded yet.</td></tr>"#
        } else {
            ""
        },
    )
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

struct UptimeMonitor;

impl Guest for UptimeMonitor {
    fn handle(request: IncomingRequest, outparam: ResponseOutparam) {
        let method = request.method();
        let path = request.path_with_query().unwrap_or_default();

        let path_only = path.split('?').next().unwrap_or(&path);
        let path_clean = path_only.trim_end_matches('/');

        match (method, path_clean) {
            // Health check
            (Method::Get, "/health" | "") => {
                let body = serde_json::json!({
                    "status": "healthy",
                    "component": "uptime-monitor",
                    "version": env!("CARGO_PKG_VERSION"),
                });
                json_response(outparam, 200, &body);
            }

            // ---- Device CRUD -------------------------------------------

            (Method::Get, "/api/devices") => {
                let devices = load_devices();
                let list: Vec<&Device> = devices.values().collect();
                json_response(outparam, 200, &list);
            }

            (Method::Post, "/api/devices") => {
                let body = read_request_body(&request);
                let req: CreateDeviceRequest = match serde_json::from_slice(&body) {
                    Ok(r) => r,
                    Err(e) => {
                        error_json(outparam, 400, &format!("invalid JSON: {e}"));
                        return;
                    }
                };

                if req.id.is_empty() || req.name.is_empty() || req.url.is_empty() {
                    error_json(outparam, 400, "id, name, and url are required");
                    return;
                }

                let device = Device {
                    id: req.id.clone(),
                    name: req.name,
                    url: req.url,
                    expected_status: req.expected_status,
                };

                let mut devices = load_devices();
                devices.insert(req.id, device.clone());
                save_devices(&devices);

                json_response(outparam, 201, &device);
            }

            (Method::Delete, path) if path.starts_with("/api/devices/") => {
                let id = &path["/api/devices/".len()..];
                let mut devices = load_devices();
                if devices.remove(id).is_some() {
                    save_devices(&devices);
                    delete_metrics(id);
                    delete_history(id);
                    let body = serde_json::json!({ "deleted": id });
                    json_response(outparam, 200, &body);
                } else {
                    error_json(outparam, 404, &format!("device '{id}' not found"));
                }
            }

            // ---- Metrics -----------------------------------------------

            (Method::Get, "/api/metrics") => {
                let devices = load_devices();
                let metrics: Vec<DeviceMetrics> =
                    devices.keys().map(|id| load_metrics(id)).collect();
                json_response(outparam, 200, &metrics);
            }

            (Method::Get, path) if path.starts_with("/api/metrics/") => {
                let id = &path["/api/metrics/".len()..];
                let devices = load_devices();
                if devices.contains_key(id) {
                    let metrics = load_metrics(id);
                    json_response(outparam, 200, &metrics);
                } else {
                    error_json(outparam, 404, &format!("device '{id}' not found"));
                }
            }

            // ---- History (raw JSON) ------------------------------------

            (Method::Get, path) if path.starts_with("/api/history/") => {
                let id = &path["/api/history/".len()..];
                let devices = load_devices();
                if devices.contains_key(id) {
                    let history = load_history(id);
                    json_response(outparam, 200, &history);
                } else {
                    error_json(outparam, 404, &format!("device '{id}' not found"));
                }
            }

            // ---- Polling -----------------------------------------------

            (Method::Post, "/api/poll") => {
                let devices = load_devices();
                // Prune old history entries for all devices
                prune_all_histories(&devices);
                let results: Vec<DeviceMetrics> =
                    devices.values().map(|d| poll_device(d)).collect();
                json_response(outparam, 200, &results);
            }

            (Method::Post, path) if path.starts_with("/api/poll/") => {
                let id = &path["/api/poll/".len()..];
                let devices = load_devices();
                match devices.get(id) {
                    Some(device) => {
                        let metrics = poll_device(device);
                        json_response(outparam, 200, &metrics);
                    }
                    None => {
                        error_json(outparam, 404, &format!("device '{id}' not found"));
                    }
                }
            }

            // ---- Dashboard data (JSON for live updates) ------------------

            (Method::Get, "/api/dashboard-data") => {
                let devices = load_devices();
                let now = now_epoch_secs();
                let mut sorted_ids: Vec<&String> = devices.keys().collect();
                sorted_ids.sort();
                let data: Vec<DashboardDevice> = sorted_ids
                    .iter()
                    .map(|id| {
                        let device = &devices[*id];
                        let metrics = load_metrics(id);
                        let history = load_history(id);
                        DashboardDevice {
                            id: device.id.clone(),
                            name: device.name.clone(),
                            url: device.url.clone(),
                            is_up: metrics.is_up,
                            uptime_percentage: metrics.uptime_percentage,
                            response_time_ms: metrics.response_time_ms,
                            last_checked: metrics.last_checked.clone(),
                            bar: compute_24h_buckets(&history, now),
                        }
                    })
                    .collect();
                json_response(outparam, 200, &data);
            }

            (Method::Get, path) if path.starts_with("/api/dashboard-data/") => {
                let id = &path["/api/dashboard-data/".len()..];
                let devices = load_devices();
                match devices.get(id) {
                    Some(device) => {
                        let metrics = load_metrics(id);
                        let history = load_history(id);
                        let now = now_epoch_secs();
                        let checks_24h = history.len() as u64;
                        let up_24h = history.iter().filter(|c| c.is_up).count() as u64;
                        let down_24h = checks_24h - up_24h;
                        let uptime_24h = if checks_24h > 0 {
                            (up_24h as f64 / checks_24h as f64) * 100.0
                        } else {
                            0.0
                        };
                        let times: Vec<u64> =
                            history.iter().filter_map(|c| c.response_time_ms).collect();
                        let avg_response_ms = if times.is_empty() {
                            None
                        } else {
                            Some(times.iter().sum::<u64>() / times.len() as u64)
                        };
                        let mut recent: Vec<CheckResult> = history.clone();
                        recent.sort_by(|a, b| b.epoch_secs.cmp(&a.epoch_secs));
                        recent.truncate(50);
                        let data = DashboardDetail {
                            id: device.id.clone(),
                            name: device.name.clone(),
                            url: device.url.clone(),
                            is_up: metrics.is_up,
                            uptime_24h,
                            uptime_all: metrics.uptime_percentage,
                            avg_response_ms,
                            checks_24h,
                            down_24h,
                            consecutive_failures: metrics.consecutive_failures,
                            total_checks: metrics.total_checks,
                            bar: compute_24h_buckets(&history, now),
                            recent_checks: recent,
                        };
                        json_response(outparam, 200, &data);
                    }
                    None => {
                        error_json(outparam, 404, &format!("device '{id}' not found"));
                    }
                }
            }

            // ---- Dashboard (HTML) ------------------------------------------

            (Method::Get, "/api/dashboard") => {
                let devices = load_devices();
                let html = render_dashboard(&devices);
                send_response(outparam, 200, "text/html; charset=utf-8", html.as_bytes());
            }

            // Detail page for a single device
            (Method::Get, path) if path.starts_with("/api/dashboard/") => {
                let id = &path["/api/dashboard/".len()..];
                let devices = load_devices();
                match devices.get(id) {
                    Some(device) => {
                        let metrics = load_metrics(id);
                        let history = load_history(id);
                        let html = render_detail_page(device, &metrics, &history);
                        send_response(outparam, 200, "text/html; charset=utf-8", html.as_bytes());
                    }
                    None => {
                        error_json(outparam, 404, &format!("device '{id}' not found"));
                    }
                }
            }

            // ---- Fallthrough -------------------------------------------

            _ => {
                error_json(outparam, 404, &format!("not found: {path_clean}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cron interface (called by the companion service component)
// ---------------------------------------------------------------------------

#[cfg(feature = "cron")]
impl CronGuest for UptimeMonitor {
    fn poll_all() {
        let devices = load_devices();
        for device in devices.values() {
            poll_device(device);
        }
    }

    fn prune() {
        let devices = load_devices();
        prune_all_histories(&devices);
    }
}

bindings::export!(UptimeMonitor with_types_in bindings);
