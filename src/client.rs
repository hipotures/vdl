//! Thin HTTP client used by the TUI and all one-shot CLI commands.
//! Clients never touch the server's filesystem directly.

use std::time::Duration;

use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppSnapshot;

#[derive(Debug, Clone)]
pub struct VdlClient {
    base_url: String,
    http: Client,
}

#[derive(Debug, Deserialize)]
pub struct AddResponse {
    pub created: bool,
    pub id: String,
    pub active: bool,
    pub state: AppSnapshot,
}

#[derive(Debug, Deserialize)]
struct ActionResponse {
    state: AppSnapshot,
}

#[derive(Debug, Deserialize)]
struct LogsResponse {
    lines: Vec<String>,
}

impl VdlClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self, String> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        let base_url = base_url.into().trim().trim_end_matches('/').to_string();
        Ok(Self { base_url, http })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn display_host(&self) -> String {
        self.base_url
            .strip_prefix("https://")
            .or_else(|| self.base_url.strip_prefix("http://"))
            .unwrap_or(&self.base_url)
            .to_string()
    }

    pub fn get_state(&self) -> Result<AppSnapshot, String> {
        let url = format!("{}/api/state", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .map_err(|e| format!("cannot reach {}: {e}", self.base_url))?;
        parse_json_response(resp)
    }

    pub fn get_logs(&self, lines: usize) -> Result<Vec<String>, String> {
        let url = format!("{}/api/logs?lines={lines}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .map_err(|e| format!("cannot reach {}: {e}", self.base_url))?;
        let parsed: LogsResponse = parse_json_response(resp)?;
        Ok(parsed.lines)
    }

    pub fn add_source(&self, source_url: &str) -> Result<AddResponse, String> {
        let url = format!("{}/api/sources", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-VDL-Request", "1")
            .json(&json!({ "url": source_url }))
            .send()
            .map_err(|e| format!("cannot reach {}: {e}", self.base_url))?;
        parse_json_response(resp)
    }

    pub fn apply_action(&self, action: &str, source_id: &str) -> Result<AppSnapshot, String> {
        let url = format!("{}/api/sources/{action}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-VDL-Request", "1")
            .json(&json!({ "id": source_id }))
            .send()
            .map_err(|e| format!("cannot reach {}: {e}", self.base_url))?;
        let parsed: ActionResponse = parse_json_response(resp)?;
        Ok(parsed.state)
    }
}

fn parse_json_response<T: for<'de> Deserialize<'de>>(
    resp: reqwest::blocking::Response,
) -> Result<T, String> {
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| format!("failed reading response: {e}"))?;
    if !status.is_success() {
        if let Ok(val) = serde_json::from_str::<Value>(&text)
            && let Some(err) = val.get("error").and_then(Value::as_str)
        {
            return Err(err.to_string());
        }
        return Err(format!("server returned HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON from server: {e}"))
}
