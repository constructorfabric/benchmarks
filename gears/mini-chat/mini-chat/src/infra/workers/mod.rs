//! Background workers of the gear: the orphan turn watchdog and the upload reaper, each running
//! its scan loop under a [`LeaderElector`] (DESIGN "Orphan Turn Watchdog", B.9.1, B.9.5).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

pub mod leader;
pub mod orphan_watchdog;
pub mod upload_reaper;

#[cfg(feature = "k8s")]
pub use leader::LeaseElector;
pub use leader::{LeaderElector, NoopElector};
pub use orphan_watchdog::OrphanWatchdog;
pub use upload_reaper::UploadReaper;

/// Most rows one scan handles; the rest are picked up by later scans (fixed, not configurable).
pub(crate) const SCAN_BATCH: u64 = 100;

/// Lets a scan stop between rows: when the gear is cancelled or this process lost the leadership
/// of the role.
#[derive(Clone)]
pub struct ScanGuard {
    cancel: CancellationToken,
    elector: Arc<dyn LeaderElector>,
    role: &'static str,
}

impl ScanGuard {
    /// A guard that never stops a scan (direct `scan_once` calls).
    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            cancel: CancellationToken::new(),
            elector: Arc::new(NoopElector),
            role: "",
        }
    }

    /// `true` while the scan may handle another row.
    pub async fn proceed(&self) -> bool {
        !self.cancel.is_cancelled() && self.elector.is_leader(self.role).await
    }
}

/// Runs `scan` every `interval` (first run immediately) while this process is the leader of
/// `role`, until `cancel`. Leadership is checked before every scan (and by the scan between
/// rows through its [`ScanGuard`]); waiting for the elector or the interval ends on `cancel`. A
/// scan in progress stops at the next row after the cancellation; a failed scan is logged by the
/// scan itself and the loop goes on.
pub(crate) async fn run_scans<F, Fut>(
    role: &'static str,
    elector: Arc<dyn LeaderElector>,
    interval: Duration,
    cancel: CancellationToken,
    mut scan: F,
) where
    F: FnMut(ScanGuard) -> Fut,
    Fut: Future<Output = ()>,
{
    let guard = ScanGuard {
        cancel: cancel.clone(),
        elector: Arc::clone(&elector),
        role,
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }
        let leader = tokio::select! {
            () = cancel.cancelled() => break,
            leader = elector.is_leader(role) => leader,
        };
        if leader {
            scan(guard.clone()).await;
        }
    }
    tracing::info!(role, "worker stopped");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use async_trait::async_trait;

    use super::*;

    struct Switch(AtomicBool);

    #[async_trait]
    impl LeaderElector for Switch {
        async fn is_leader(&self, _role: &str) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn scans_run_only_while_leader_and_stop_on_cancel() {
        let elector = Arc::new(Switch(AtomicBool::new(false)));
        let scans = Arc::new(AtomicU32::new(0));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_scans(
            "test",
            elector.clone(),
            Duration::from_secs(10),
            cancel.clone(),
            {
                let scans = Arc::clone(&scans);
                move |_guard| {
                    let scans = Arc::clone(&scans);
                    async move {
                        scans.fetch_add(1, Ordering::SeqCst);
                    }
                }
            },
        ));

        tokio::time::sleep(Duration::from_secs(35)).await;
        assert_eq!(scans.load(Ordering::SeqCst), 0, "not the leader");

        elector.0.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert!(
            scans.load(Ordering::SeqCst) >= 2,
            "leader scans every interval"
        );

        cancel.cancel();
        task.await.expect("the loop ends on cancel");
        let after = scans.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(scans.load(Ordering::SeqCst), after, "no scan after cancel");
    }

    #[tokio::test(start_paused = true)]
    async fn guard_stops_a_scan_when_leadership_is_lost_or_on_cancel() {
        let elector = Arc::new(Switch(AtomicBool::new(true)));
        let cancel = CancellationToken::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let task = tokio::spawn(run_scans(
            "test",
            elector.clone(),
            Duration::from_secs(10),
            cancel.clone(),
            {
                let (seen, elector) = (Arc::clone(&seen), Arc::clone(&elector));
                move |guard: ScanGuard| {
                    let (seen, elector) = (Arc::clone(&seen), Arc::clone(&elector));
                    async move {
                        let mut verdicts = vec![guard.proceed().await];
                        elector.0.store(false, Ordering::SeqCst);
                        verdicts.push(guard.proceed().await);
                        seen.lock().unwrap().push(verdicts);
                    }
                }
            },
        ));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*seen.lock().unwrap(), vec![vec![true, false]]);

        elector.0.store(true, Ordering::SeqCst);
        assert!(ScanGuard::unrestricted().proceed().await);
        cancel.cancel();
        task.await.unwrap();
    }
}
