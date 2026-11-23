//! Shared snapshots and stable-ID operations for the terminal and HTTP API.

use serde::Serialize;

use crate::domain::now_unix;
use crate::repository::{RepoError, SourceRepository, display_service};
use crate::runtime::{FileLock, current_download, lock_is_held};

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct AppSnapshot {
    pub sources: Vec<SourceSnapshot>,
    pub download: Option<String>,
    pub owner_running: bool,
    pub pending: usize,
    pub time: f64,
}

pub fn snapshot(repository: &SourceRepository) -> Result<AppSnapshot, RepoError> {
    let (rows, busy, timestamp) = {
        let _lock = FileLock::acquire(&repository.runtime.mutation_lock, true)?;
        let busy = current_download(&repository.runtime);
        let sources = repository.list_sources();
        let timestamp = now_unix();
        let mut rows = Vec::with_capacity(sources.len());
        for source in sources {
            let id = source.id();
            let running = busy.as_deref() == Some(id.as_str());
            let pending =
                source.active && (source.download_requested || source.last_check.is_none());
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
        (rows, busy, timestamp)
    };

    let pending = rows.iter().filter(|r| r.state == "queued").count();
    let owner_running = lock_is_held(&repository.runtime.owner_lock);
    Ok(AppSnapshot {
        sources: rows,
        download: busy,
        owner_running,
        pending,
        time: timestamp,
    })
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
