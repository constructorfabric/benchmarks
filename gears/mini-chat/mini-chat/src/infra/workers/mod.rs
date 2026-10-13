//! Background tasks: upload background indexing (request-spawned) and the
//! periodic leader-only workers (orphan watchdog, upload reaper).

pub mod background_indexing;
pub mod leader;
pub mod orphan_watchdog;
pub mod upload_reaper;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use leader::LeaderElector;

/// Call `scan` every `interval` (first call at once) while this process
/// leads `role`, until `cancel` fires (an in-flight scan is dropped; its
/// open transaction rolls back).
pub async fn run_periodic<F, Fut>(
    role: &'static str,
    interval: Duration,
    elector: Arc<dyn LeaderElector>,
    cancel: CancellationToken,
    mut scan: F,
) where
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = ()> + Send,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        if !elector.is_leader(role) {
            continue;
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = scan() => {}
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
