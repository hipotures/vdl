//! Headless owner process for the single vdl scheduler.

use std::io;
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};

use crate::config::Config;
use crate::repository::SourceRepository;
use crate::runtime::{FileLock, RuntimePaths, lock_is_held, set_busy};
use crate::scheduler::Scheduler;

pub fn run_owner(config: Config) -> Result<(), String> {
    let runtime = RuntimePaths::resolve().map_err(|e| e.to_string())?;
    let _owner_lock = match FileLock::acquire(&runtime.owner_lock, false) {
        Ok(lock) => lock,
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            return Err("another vdl owner is already running".to_string());
        }
        Err(err) => return Err(err.to_string()),
    };

    if !lock_is_held(&runtime.download_lock) {
        let _ = set_busy(&runtime, None);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    rt.block_on(async move {
        let repository = SourceRepository::with_runtime(&config.download_root, runtime);
        let scheduler = Arc::new(Scheduler::new(repository, &config).map_err(|e| e.to_string())?);
        let task = scheduler.spawn();

        let mut sigint = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
        let mut sigterm = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;

        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }

        task.abort();
        let _ = task.await;
        Ok(())
    })
}
