//! One downloader, with pending work inferred from filesystem markers.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::domain::now_unix;
use crate::repository::{Source, SourceRepository};
use crate::runner::{AppLogger, YtDlpRunner};
use crate::runtime::{FileLock, set_busy};

pub fn source_is_due(source: &Source, now: f64, check_interval: f64) -> bool {
    match source.last_check {
        None => true,
        Some(last) => now - last >= check_interval,
    }
}

fn pending_key(source: &Source) -> (f64, (String, String, String)) {
    (source.queued_at.unwrap_or(0.0), source.sort_key())
}

fn cmp_f64_then_key(
    a: (f64, (String, String, String)),
    b: (f64, (String, String, String)),
) -> std::cmp::Ordering {
    a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1))
}

pub fn select_due_source(
    sources: &[Source],
    now: f64,
    check_interval: f64,
    minimum_spacing: f64,
) -> Option<Source> {
    let active: Vec<&Source> = sources.iter().filter(|s| s.active).collect();

    if let Some(best) = active
        .iter()
        .copied()
        .filter(|s| s.download_requested)
        .min_by(|a, b| cmp_f64_then_key(pending_key(a), pending_key(b)))
    {
        return Some(best.clone());
    }

    if let Some(best) = active
        .iter()
        .copied()
        .filter(|s| s.last_check.is_none())
        .min_by(|a, b| cmp_f64_then_key(pending_key(a), pending_key(b)))
    {
        return Some(best.clone());
    }

    let overdue: Vec<&Source> = active
        .iter()
        .copied()
        .filter(|s| source_is_due(s, now, check_interval))
        .collect();
    if overdue.is_empty() {
        return None;
    }

    let latest = sources
        .iter()
        .filter_map(|s| s.last_check)
        .fold(f64::NEG_INFINITY, f64::max);
    if now - latest < minimum_spacing {
        return None;
    }

    overdue
        .into_iter()
        .min_by(|a, b| {
            let ka = (a.last_check.unwrap_or(0.0), a.sort_key());
            let kb = (b.last_check.unwrap_or(0.0), b.sort_key());
            cmp_f64_then_key(ka, kb)
        })
        .cloned()
}

pub struct Scheduler {
    repository: SourceRepository,
    check_interval: f64,
    minimum_spacing: f64,
    scheduler_poll: Duration,
    runner: Arc<YtDlpRunner>,
    logger: Arc<AppLogger>,
    worker_lock: Arc<Mutex<()>>,
    wake: Arc<Notify>,
}

impl Scheduler {
    pub fn new(repository: SourceRepository, config: &Config) -> std::io::Result<Self> {
        let logger = Arc::new(AppLogger::new(Some(&config.log_file)));
        let runner = Arc::new(YtDlpRunner::new(config, Arc::clone(&logger))?);
        let poll_secs = config.scheduler_poll.clamp(0.01, 1.0);
        Ok(Self {
            repository,
            check_interval: config.check_interval,
            minimum_spacing: config.minimum_spacing,
            scheduler_poll: Duration::from_secs_f64(poll_secs),
            runner,
            logger,
            worker_lock: Arc::new(Mutex::new(())),
            wake: Arc::new(Notify::new()),
        })
    }

    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run_loop().await;
        })
    }

    pub async fn run_once(&self) -> Option<Source> {
        let Ok(_worker) = self.worker_lock.try_lock() else {
            return None;
        };
        let Ok(download_lock) = FileLock::acquire(&self.repository.runtime.download_lock, false)
        else {
            return None;
        };
        let _ = set_busy(&self.repository.runtime, None);

        let repo = self.repository.clone();
        let check_interval = self.check_interval;
        let minimum_spacing = self.minimum_spacing;
        let claimed = tokio::task::spawn_blocking(move || {
            repo.claim_next(|sources| {
                select_due_source(sources, now_unix(), check_interval, minimum_spacing)
            })
        })
        .await;

        let source = match claimed {
            Ok(Ok(Some(source))) => source,
            Ok(Ok(None)) => return None,
            Ok(Err(err)) => {
                self.logger.error(&format!("scheduler claim error: {err}"));
                return None;
            }
            Err(err) => {
                self.logger.error(&format!("scheduler join error: {err}"));
                return None;
            }
        };

        self.logger.info(&format!(
            "source selected source={} url={}",
            source.path.display(),
            source.url
        ));
        let fd = download_lock.as_raw_fd();
        if let Err(err) = self.runner.run(&source, Some(fd)).await {
            self.logger.error(&format!(
                "source failed source={}: {err}",
                source.path.display()
            ));
        }
        let _ = set_busy(&self.repository.runtime, None);
        drop(download_lock);
        Some(source)
    }

    async fn run_loop(&self) {
        loop {
            let completed = self.run_once().await;
            if completed.is_some() {
                tokio::task::yield_now().await;
                continue;
            }
            let _ = tokio::time::timeout(self.scheduler_poll, self.wake.notified()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fake(name: &str, last_check: Option<f64>, active: bool, requested: bool) -> Source {
        Source {
            service: "site".to_string(),
            account: name.to_string(),
            path: PathBuf::from("/tmp/vdl-test").join(name),
            url: format!("https://example.test/{name}"),
            active,
            last_check,
            download_requested: requested,
            queued_at: None,
        }
    }

    #[test]
    fn due_selection_handles_new_oldest_and_spacing() {
        let now = 1000.0;
        let newest = fake("newest", Some(900.0), true, false);
        let oldest = fake("oldest", Some(500.0), true, false);
        let middle = fake("middle", Some(650.0), true, false);
        let new = fake("new", None, true, false);
        let inactive = fake("inactive", Some(100.0), false, false);
        let sources = vec![
            newest.clone(),
            inactive.clone(),
            middle.clone(),
            new.clone(),
            oldest.clone(),
        ];
        let requested = fake("requested", Some(999.0), true, true);

        assert!(source_is_due(&new, now, 10000.0));
        assert!(source_is_due(&oldest, now, 200.0));
        assert!(!source_is_due(&newest, now, 200.0));

        assert_eq!(
            select_due_source(&sources, now, 200.0, 10000.0)
                .unwrap()
                .account,
            "new"
        );
        let mut with_req = sources.clone();
        with_req.push(requested);
        assert_eq!(
            select_due_source(&with_req, now, 10000.0, 10000.0)
                .unwrap()
                .account,
            "requested"
        );

        let without_new: Vec<Source> = sources.into_iter().filter(|s| s.account != "new").collect();
        assert!(select_due_source(&without_new, now, 200.0, 150.0).is_none());
        assert_eq!(
            select_due_source(&without_new, now, 200.0, 100.0)
                .unwrap()
                .account,
            "oldest"
        );

        let disabled = fake("disabled-now", Some(950.0), false, false);
        assert!(select_due_source(&[oldest, middle, disabled], now, 200.0, 100.0).is_none());
    }
}
