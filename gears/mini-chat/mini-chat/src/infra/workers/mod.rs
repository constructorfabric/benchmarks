//! Periodic background workers: orphan watchdog and upload reaper.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::services::MiniChatService;

/// Spawns the orphan watchdog loop (if enabled).
#[must_use]
pub fn spawn_orphan_watchdog(svc: Arc<MiniChatService>, cancel: CancellationToken) -> Option<JoinHandle<()>> {
    let cfg = svc.config().orphan_watchdog.clone();
    if !cfg.enabled {
        return None;
    }
    let every = Duration::from_secs(cfg.scan_interval_secs.max(1));
    Some(tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(every) => {}
            }
            let n = svc.orphan_scan().await;
            if n > 0 {
                tracing::info!(finalized = n, "orphan watchdog finalized stale turns");
            }
        }
    }))
}

/// Spawns the upload reaper loop (if enabled).
#[must_use]
pub fn spawn_upload_reaper(svc: Arc<MiniChatService>, cancel: CancellationToken) -> Option<JoinHandle<()>> {
    let cfg = svc.config().upload_reaper.clone();
    if !cfg.enabled {
        return None;
    }
    let every = Duration::from_secs(cfg.scan_interval_secs.max(1));
    Some(tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(every) => {}
            }
            let n = svc.upload_reaper_scan().await;
            if n > 0 {
                tracing::info!(reaped = n, "upload reaper marked abandoned uploads");
            }
        }
    }))
}
