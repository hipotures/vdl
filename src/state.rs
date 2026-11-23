//! Shared snapshots and stable-ID operations for the HTTP API and clients.

use serde::{Deserialize, Serialize};

use crate::domain::now_unix;
use crate::repository::{RepoError, SourceRepository, display_service};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub id: String,
    pub service: String,
    pub account: String,
    pub url: String,
    pub active: bool,
    pub state: String,
    pub download_requested: bool,
    pub last_check: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSnapshot {
    pub sources: Vec<SourceSnapshot>,
    pub download: Option<String>,
    pub owner_running: bool,
    pub pending: usize,
    pub time: f64,
}

pub fn snapshot(repository: &SourceRepository) -> AppSnapshot {
    let (sources, busy) = repository.snapshot_sources();
    let timestamp = now_unix();
    let mut rows = Vec::with_capacity(sources.len());
    for source in sources {
        let id = source.id();
        let running = busy.as_deref() == Some(id.as_str());
        let pending = source.active && (source.download_requested || source.last_check.is_none());
        let state = if running {
            if source.active {
                "downloading"
            } else {
                "finishing"
            }
        } else if pending {
            "queued"
        } else if source.active {
            "active"
        } else {
            "inactive"
        };
        rows.push(SourceSnapshot {
            id,
            service: display_service(&source.service),
            account: source.account,
            url: source.url,
            active: source.active,
            state: state.to_string(),
            download_requested: source.download_requested,
            last_check: source.last_check,
        });
    }
    let pending = rows.iter().filter(|r| r.state == "queued").count();
    AppSnapshot {
        sources: rows,
        download: busy,
        owner_running: true,
        pending,
        time: timestamp,
    }
}

pub fn apply_action(
    repository: &SourceRepository,
    action: &str,
    source_id: &str,
) -> Result<(), RepoError> {
    if action != "disable" && action != "download-now" {
        return Err(RepoError::Value("unknown source action".to_string()));
    }
    let sources = repository.list_sources();
    let Some(source) = sources.into_iter().find(|item| item.id() == source_id) else {
        return Err(RepoError::NotFound(
            "source no longer exists; refresh and try again".to_string(),
        ));
    };
    if action == "disable" {
        repository.disable(&source)?;
    } else {
        repository.request_download(&source)?;
    }
    Ok(())
}
