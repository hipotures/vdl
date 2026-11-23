//! Small TOML configuration loader for vdl.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_CHECK_INTERVAL: f64 = 24.0 * 60.0 * 60.0;
pub const DEFAULT_MINIMUM_SPACING: f64 = 10.0 * 60.0;
pub const DEFAULT_SCHEDULER_POLL: f64 = 60.0;
pub const DEFAULT_LOG_FILE: &str = "/tmp/vdl/vdl.log";
pub const DEFAULT_WEB_HOST: &str = "0.0.0.0";
pub const DEFAULT_WEB_PORT: u16 = 8780;

pub const DEFAULT_CONFIG_TEXT: &str = r#"download_root = "~/Downloads/vdl"

check_interval = "24h"
minimum_spacing = "10m"
scheduler_poll = "1m"

log_file = "/tmp/vdl/vdl.log"

web_host = "0.0.0.0"
web_port = 8780

yt_dlp_command = [
    "yt-dlp",
    "--download-archive", ".archive"
]
"#;

#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub download_root: PathBuf,
    pub check_interval: f64,
    pub minimum_spacing: f64,
    pub scheduler_poll: f64,
    pub log_file: PathBuf,
    pub web_host: String,
    pub web_port: u16,
    pub server_url: Option<String>,
    pub yt_dlp_command: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_root: expand_tilde("~/Downloads/vdl"),
            check_interval: DEFAULT_CHECK_INTERVAL,
            minimum_spacing: DEFAULT_MINIMUM_SPACING,
            scheduler_poll: DEFAULT_SCHEDULER_POLL,
            log_file: PathBuf::from(DEFAULT_LOG_FILE),
            web_host: DEFAULT_WEB_HOST.to_string(),
            web_port: DEFAULT_WEB_PORT,
            server_url: None,
            yt_dlp_command: vec![
                "yt-dlp".to_string(),
                "--download-archive".to_string(),
                ".archive".to_string(),
            ],
        }
    }
}

impl Config {
    /// Resolve the HTTP base URL clients should connect to.
    pub fn resolved_server_url(&self, cli_override: Option<&str>) -> String {
        let raw = cli_override
            .map(str::to_string)
            .or_else(|| {
                std::env::var("VDL_SERVER")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| self.server_url.clone())
            .unwrap_or_else(|| {
                let host = if self.web_host == "0.0.0.0" || self.web_host == "::" {
                    "127.0.0.1"
                } else {
                    self.web_host.as_str()
                };
                format!("http://{host}:{}", self.web_port)
            });
        raw.trim().trim_end_matches('/').to_string()
    }
}

pub fn default_config_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("VDL_CONFIG")
        && !env_path.trim().is_empty()
    {
        return expand_tilde(env_path.trim());
    }
    expand_tilde("~/.config/vdl/config.toml")
}

pub fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

pub fn expand_path(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(text) => expand_tilde(text),
        None => path.to_path_buf(),
    }
}

pub fn parse_duration_str(value: &str) -> Result<f64, ConfigError> {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 2 {
        return Err(ConfigError(format!(
            "invalid duration '{value}'; expected a number followed by s, m, h, or d"
        )));
    }
    let unit = (bytes[bytes.len() - 1] as char).to_ascii_lowercase();
    let factor = match unit {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86400.0,
        _ => {
            return Err(ConfigError(format!(
                "invalid duration '{value}'; expected a number followed by s, m, h, or d"
            )));
        }
    };
    let num_part = trimmed[..trimmed.len() - 1].trim_end();
    if num_part.is_empty() || !is_valid_unsigned_decimal(num_part) {
        return Err(ConfigError(format!(
            "invalid duration '{value}'; expected a number followed by s, m, h, or d"
        )));
    }
    let amount: f64 = num_part.parse().map_err(|_| {
        ConfigError(format!(
            "invalid duration '{value}'; expected a number followed by s, m, h, or d"
        ))
    })?;
    Ok(amount * factor)
}

fn is_valid_unsigned_decimal(s: &str) -> bool {
    let mut parts = s.split('.');
    let Some(int_part) = parts.next() else {
        return false;
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(frac) => {
            parts.next().is_none() && !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

fn parse_duration_value(val: &toml::Value) -> Result<f64, ConfigError> {
    match val {
        toml::Value::Boolean(_) => Err(ConfigError(
            "duration must be a string such as '10m'".to_string(),
        )),
        toml::Value::Integer(n) => {
            if *n < 0 {
                return Err(ConfigError("duration cannot be negative".to_string()));
            }
            Ok(*n as f64)
        }
        toml::Value::Float(f) => {
            if *f < 0.0 {
                return Err(ConfigError("duration cannot be negative".to_string()));
            }
            Ok(*f)
        }
        toml::Value::String(s) => parse_duration_str(s),
        _ => Err(ConfigError(
            "duration must be a string such as '10m'".to_string(),
        )),
    }
}

fn path_value(val: &toml::Value, field: &str) -> Result<PathBuf, ConfigError> {
    match val.as_str() {
        Some(s) if !s.trim().is_empty() => Ok(expand_tilde(s)),
        _ => Err(ConfigError(format!(
            "{field} must be a non-empty path string"
        ))),
    }
}

pub fn config_from_table(table: &toml::Table) -> Result<Config, ConfigError> {
    let defaults = Config::default();

    let download_root = match table.get("download_root") {
        Some(v) => path_value(v, "download_root")?,
        None => defaults.download_root,
    };
    let log_file = match table.get("log_file") {
        Some(v) => path_value(v, "log_file")?,
        None => defaults.log_file,
    };
    let web_host = match table.get("web_host") {
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Err(ConfigError(
                    "web_host must be a non-empty string".to_string(),
                ));
            }
        },
        None => defaults.web_host,
    };
    let web_port = match table.get("web_port") {
        Some(v) => match v {
            toml::Value::Integer(n) if (1..=65535).contains(n) => *n as u16,
            _ => {
                return Err(ConfigError(
                    "web_port must be an integer between 1 and 65535".to_string(),
                ));
            }
        },
        None => defaults.web_port,
    };
    let server_url = match table.get("server_url") {
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => {
                return Err(ConfigError(
                    "server_url must be a non-empty string".to_string(),
                ));
            }
        },
        None => None,
    };
    let check_interval = match table.get("check_interval") {
        Some(v) => parse_duration_value(v)?,
        None => defaults.check_interval,
    };
    let minimum_spacing = match table.get("minimum_spacing") {
        Some(v) => parse_duration_value(v)?,
        None => defaults.minimum_spacing,
    };
    let scheduler_poll = match table.get("scheduler_poll") {
        Some(v) => parse_duration_value(v)?,
        None => defaults.scheduler_poll,
    };
    let yt_dlp_command = match table.get("yt_dlp_command") {
        Some(v) => {
            let Some(arr) = v.as_array() else {
                return Err(ConfigError(
                    "yt_dlp_command must be a non-empty array of strings".to_string(),
                ));
            };
            if arr.is_empty() {
                return Err(ConfigError(
                    "yt_dlp_command must be a non-empty array of strings".to_string(),
                ));
            }
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let Some(s) = item.as_str() else {
                    return Err(ConfigError(
                        "yt_dlp_command must be a non-empty array of strings".to_string(),
                    ));
                };
                if s.is_empty() {
                    return Err(ConfigError(
                        "yt_dlp_command must be a non-empty array of strings".to_string(),
                    ));
                }
                out.push(s.to_string());
            }
            out
        }
        None => defaults.yt_dlp_command,
    };

    Ok(Config {
        download_root,
        check_interval,
        minimum_spacing,
        scheduler_poll,
        log_file,
        web_host,
        web_port,
        server_url,
        yt_dlp_command,
    })
}

fn apply_env_overrides(mut config: Config) -> Result<Config, ConfigError> {
    if let Ok(root) = std::env::var("VDL_DOWNLOAD_ROOT")
        && !root.trim().is_empty()
    {
        config.download_root = expand_tilde(root.trim());
    }
    if let Ok(log) = std::env::var("VDL_LOG_FILE")
        && !log.trim().is_empty()
    {
        config.log_file = expand_tilde(log.trim());
    }
    if let Ok(host) = std::env::var("VDL_HOST")
        && !host.trim().is_empty()
    {
        config.web_host = host.trim().to_string();
    }
    if let Ok(port_str) = std::env::var("VDL_PORT")
        && !port_str.trim().is_empty()
    {
        let port: u16 = port_str
            .trim()
            .parse()
            .ok()
            .filter(|&p| p >= 1)
            .ok_or_else(|| {
                ConfigError("VDL_PORT must be an integer between 1 and 65535".to_string())
            })?;
        config.web_port = port;
    }
    Ok(config)
}

pub fn load_config(path: Option<&Path>) -> Result<Config, ConfigError> {
    let config_path = match path {
        Some(p) => expand_path(p),
        None => default_config_path(),
    };
    let config = if !config_path.exists() {
        Config::default()
    } else {
        let text = fs::read_to_string(&config_path)
            .map_err(|e| ConfigError(format!("cannot read {}: {e}", config_path.display())))?;
        let table: toml::Table = text
            .parse()
            .map_err(|e| ConfigError(format!("invalid TOML in {}: {e}", config_path.display())))?;
        config_from_table(&table)?
    };
    apply_env_overrides(config)
}

pub fn ensure_config(path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let config_path = match path {
        Some(p) => expand_path(p),
        None => default_config_path(),
    };
    if !config_path.exists() {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ConfigError(format!("cannot create {}: {e}", parent.display())))?;
        }
        fs::write(&config_path, DEFAULT_CONFIG_TEXT)
            .map_err(|e| ConfigError(format!("cannot write {}: {e}", config_path.display())))?;
    }
    Ok(config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_durations() {
        assert_eq!(parse_duration_str("30s").unwrap(), 30.0);
        assert_eq!(parse_duration_str("10m").unwrap(), 600.0);
        assert_eq!(parse_duration_str("24h").unwrap(), 86400.0);
        assert_eq!(parse_duration_str("2d").unwrap(), 172800.0);
        assert_eq!(parse_duration_str("1.5h").unwrap(), 5400.0);
        assert!(parse_duration_str("5x").is_err());
        assert!(parse_duration_str("-5s").is_err());
    }
}
